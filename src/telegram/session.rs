//! One agent: its bot token, its identity, and its poller.
//!
//! Every session shares the process-wide [`Hub`], which is what lets agents
//! see each other's messages despite the Bot API refusing to deliver
//! bot-authored messages between bots.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

use super::api::TelegramApi;
use super::hub::{Hub, WaitFilters};
use super::lock;
use super::model::{
    AgentSelfProfile, BotIdentity, ChatSummary, ConversationView, KnownAgent, PROFILE_MARKER,
    ProfilePayload, RawChat, RawUpdate, RawUser, SimpleMessage, extract_mentions,
    find_profile_json, split_text,
};
use super::view::Viewer;

/// How long each `getUpdates` long-poll waits before returning empty.
const POLL_TIMEOUT_SECS: u64 = 30;
const POLL_BATCH_LIMIT: u32 = 100;
/// Backoff after a failed poll, so a persistent error does not spin.
const POLL_ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// The name an MCP client uses to select this agent (`?agent=<id>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(String);

impl AgentId {
    /// Names are lowercased so `?agent=Qwen` and `?agent=qwen` are the same
    /// agent, and restricted to URL-safe characters since they appear in a
    /// route path.
    pub fn parse(raw: &str) -> Result<Self> {
        let name = raw.trim().to_ascii_lowercase();
        if name.is_empty() {
            anyhow::bail!("agent name must not be empty");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!(
                "agent name {raw:?} must contain only letters, digits, '-' or '_' \
                 (it appears in a URL)"
            );
        }
        Ok(Self(name))
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct AgentSession {
    id: AgentId,
    hub: Arc<Hub>,
    api: TelegramApi,
    profile: AgentSelfProfile,
    identity: OnceCell<BotIdentity>,
    /// getUpdates offset. Strictly per-token: sharing it across bots would
    /// silently drop updates.
    last_update_id: std::sync::Mutex<i64>,
}

impl AgentSession {
    /// `http` is shared across every agent so they use one connection pool.
    /// It must be built after a rustls crypto provider is installed.
    pub fn new(
        id: AgentId,
        hub: Arc<Hub>,
        http: reqwest::Client,
        token: String,
        profile: AgentSelfProfile,
    ) -> Self {
        let api = TelegramApi::new(http, token);
        Self {
            id,
            hub,
            api,
            profile,
            identity: OnceCell::new(),
            last_update_id: std::sync::Mutex::new(0),
        }
    }

    /// The configured name for this agent, i.e. the `?agent=` key.
    pub fn id(&self) -> &AgentId {
        &self.id
    }

    /// Fetches (and caches) this bot's own identity via getMe.
    pub async fn get_me(&self) -> Result<BotIdentity> {
        self.identity
            .get_or_try_init(|| async {
                let me: RawUser = self.api.call("getMe", &serde_json::json!({}), None).await?;
                Ok(BotIdentity {
                    id: me.id,
                    username: me.username,
                    first_name: me.first_name,
                })
            })
            .await
            .cloned()
    }

    /// This agent's identity as a [`Viewer`]. Falls back to an empty viewer if
    /// `getMe` has not succeeded yet, which marks nothing as self rather than
    /// marking everything.
    pub async fn viewer(&self) -> Viewer {
        Viewer::new(self.get_me().await.ok().as_ref())
    }

    /// Renders messages as `#seq` thread entries, the same shape
    /// `get_conversation` returns, so every tool reads alike.
    pub async fn render(&self, messages: &[SimpleMessage]) -> String {
        let viewer = self.viewer().await;
        self.hub.render_messages(messages, &viewer)
    }

    // -----------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------

    /// Sends `text` to `chat_id`, transparently splitting it into multiple
    /// messages if it exceeds Telegram's length limit (later chunks are sent
    /// as replies to the previous chunk, to keep them visibly threaded
    /// together). Returns every message actually sent, in order.
    ///
    /// Each sent message is recorded in the shared hub, which is how other
    /// agents in this process come to see it — Telegram itself will never
    /// deliver it to them.
    ///
    /// `reply_to_message_id` has the same visibility problem as everything
    /// else in this file: Telegram's `sendMessage` validates the reply target
    /// against this bot's own update stream, which never includes another
    /// bot's messages, so a reply to another agent is rejected outright. When
    /// that happens this retries as a plain send but *still records the
    /// intended `reply_to_message_id`* on the hub-side message — the reply
    /// context `get_conversation` renders and the addressing `wait_for_reply`
    /// checks are both driven by that stored field, not by whether Telegram's
    /// UI actually drew a thread line, so the only real loss is cosmetic.
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_to_message_id: Option<i64>,
    ) -> Result<Vec<SimpleMessage>> {
        let me = self.get_me().await.ok();
        let chunks = split_text(text);
        let mut sent = Vec::with_capacity(chunks.len());
        let mut reply_id = reply_to_message_id;

        for chunk in chunks {
            let with_reply = |r: Option<i64>| {
                let mut body = serde_json::json!({ "chat_id": chat_id, "text": chunk });
                if let Some(r) = r {
                    body["reply_parameters"] = serde_json::json!({ "message_id": r });
                }
                body
            };

            let raw: super::model::RawMessage = match self
                .api
                .call("sendMessage", &with_reply(reply_id), None)
                .await
            {
                Ok(raw) => raw,
                Err(err) if reply_id.is_some() => self
                    .api
                    .call("sendMessage", &with_reply(None), None)
                    .await
                    .with_context(|| {
                        format!(
                            "retried without reply_to_message_id after the reply failed \
                             ({err:#}), and that also failed"
                        )
                    })?,
                Err(err) => return Err(err),
            };
            self.hub.remember_chat(&raw.chat);

            let msg = SimpleMessage {
                seq: 0,
                message_id: raw.message_id,
                chat_id: raw.chat.id,
                date: raw.date,
                from_id: me.as_ref().map(|m| m.id),
                from: me
                    .as_ref()
                    .map(|m| m.username.clone().unwrap_or_else(|| m.first_name.clone())),
                from_username: me.as_ref().and_then(|m| m.username.clone()),
                from_is_bot: true,
                from_is_human: false,
                edit_date: None,
                is_announcement: find_profile_json(&chunk).is_some(),
                mentions: extract_mentions(&chunk),
                reply_to_message_id: reply_id,
                text: Some(chunk),
            };
            let logged = self.hub.record_sent(msg);
            reply_id = Some(logged.message_id);
            sent.push(logged);
        }

        Ok(sent)
    }

    /// Broadcasts this agent's identity/capabilities into `chat_id` so other
    /// agents can discover it via `list_agents`. Any field left `None` falls
    /// back to this agent's configured defaults, then is omitted if still
    /// unset.
    pub async fn announce(
        &self,
        chat_id: i64,
        name: Option<String>,
        model: Option<String>,
        description: Option<String>,
    ) -> Result<Vec<SimpleMessage>> {
        let me = self.get_me().await?;
        let payload = ProfilePayload {
            name: name
                .or_else(|| self.profile.name.clone())
                .or_else(|| me.username.clone())
                .or(Some(me.first_name.clone())),
            model: model.or_else(|| self.profile.model.clone()),
            description: description.or_else(|| self.profile.description.clone()),
        };
        // A human reading the chat directly sees this summary line, not the
        // machine payload — that lives on its own line, found by
        // `find_profile_json` rather than requiring it to open the message.
        let who = payload.name.as_deref().unwrap_or("an agent");
        let extra: Vec<&str> = [payload.model.as_deref(), payload.description.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        let summary = if extra.is_empty() {
            format!("🤖 {who} joined the chat")
        } else {
            format!("🤖 {who} joined the chat — {}", extra.join(" — "))
        };
        let text = format!(
            "{summary}\n{PROFILE_MARKER}{}",
            serde_json::to_string(&payload).context("serializing agent profile")?
        );
        self.send_message(chat_id, &text, None).await
    }

    /// Puts a native Telegram reaction on a *person's* message — a read
    /// receipt, not a reply.
    ///
    /// This deliberately has no fallback. Telegram only lets a bot react to a
    /// message its own update stream delivered, which never includes another
    /// bot's messages, and the obvious workaround — posting the emoji as a
    /// message instead — turns out to be worse than nothing: it puts a bare
    /// "👀" in the transcript, consuming a turn and everyone's attention while
    /// saying nothing. Between agents, a one-line reply is both cheaper and
    /// actually informative, so that case returns an error pointing there.
    pub async fn react(&self, chat_id: i64, message_id: i64, emoji: Option<&str>) -> Result<()> {
        let reaction = emoji
            .map(|e| serde_json::json!([{ "type": "emoji", "emoji": e }]))
            .unwrap_or_else(|| serde_json::json!([]));
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": reaction,
        });
        self.api
            .call::<bool>("setMessageReaction", &body, None)
            .await
            .map(|_| ())
            .map_err(|err| {
                anyhow::anyhow!(
                    "could not react to that message ({err:#}). Telegram only allows reacting to \
                     messages this bot can see, which excludes other agents' messages. If you \
                     were acknowledging another agent, reply to them with one short line \
                     instead — say what you're doing, not just that you saw it."
                )
            })
    }

    // -----------------------------------------------------------------
    // Polling
    // -----------------------------------------------------------------

    /// Poll getUpdates once (long-poll up to `timeout_secs`), ingesting
    /// anything new into the shared hub. Returns only the messages this call
    /// actually ingested — copies another agent's poller already recorded are
    /// deduplicated away and not returned.
    pub async fn poll_updates(&self, timeout_secs: u64, limit: u32) -> Result<Vec<SimpleMessage>> {
        let offset = { *lock(&self.last_update_id) + 1 };

        let body = serde_json::json!({
            "offset": offset,
            "timeout": timeout_secs,
            "limit": limit,
            "allowed_updates": ["message", "edited_message", "channel_post"],
        });
        let updates: Vec<RawUpdate> = self
            .api
            .call(
                "getUpdates",
                &body,
                Some(Duration::from_secs(timeout_secs + 10)),
            )
            .await?;

        let mut messages = Vec::new();
        let mut max_update_id = offset - 1;

        for update in updates {
            max_update_id = max_update_id.max(update.update_id);
            let raw = update
                .message
                .or(update.edited_message)
                .or(update.channel_post);
            if let Some(msg) = raw {
                let chat_id = msg.chat.id;
                if let Some(stored) = self.hub.ingest_raw(msg) {
                    messages.push(stored);
                }
                self.hub.warn_if_dedup_unreliable(chat_id);
            }
        }

        *lock(&self.last_update_id) = max_update_id;
        Ok(messages)
    }

    /// Runs `poll_updates` forever, logging errors and backing off briefly on
    /// failure instead of dying. Spawned as a background task so incoming
    /// messages are cached even when no tool call is in flight.
    pub async fn run_poller_forever(self: Arc<Self>) {
        if let Err(err) = self.get_me().await {
            tracing::warn!(agent = %self.id, "getMe failed (will retry lazily): {err:#}");
        }
        loop {
            match self.poll_updates(POLL_TIMEOUT_SECS, POLL_BATCH_LIMIT).await {
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(agent = %self.id, "getUpdates poll failed: {err:#}");
                    tokio::time::sleep(POLL_ERROR_BACKOFF).await;
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Reads, with this agent's viewpoint applied
    // -----------------------------------------------------------------

    pub async fn get_chat(&self, chat_id: i64) -> Result<ChatSummary> {
        let chat: RawChat = self
            .api
            .call("getChat", &serde_json::json!({ "chat_id": chat_id }), None)
            .await?;
        self.hub.remember_chat(&chat);
        self.hub
            .chat_summary(chat_id)
            .context("chat not found after lookup")
    }

    pub fn list_known_chats(&self) -> Vec<ChatSummary> {
        self.hub.list_known_chats()
    }

    pub fn list_known_agents(&self) -> Vec<KnownAgent> {
        self.hub.list_known_agents()
    }

    pub fn search_messages(
        &self,
        query: &str,
        chat_id: Option<i64>,
        limit: usize,
    ) -> Vec<SimpleMessage> {
        self.hub.search_messages(query, chat_id, limit)
    }

    pub async fn get_conversation(
        &self,
        chat_id: i64,
        limit: usize,
        include_announcements: bool,
        include_json: bool,
    ) -> ConversationView {
        let viewer = self.viewer().await;
        self.hub
            .render_conversation(chat_id, limit, include_announcements, include_json, &viewer)
    }

    /// Resolves an optional `chat_id` argument, falling back to the only chat
    /// this server knows about. See [`Hub::default_chat`].
    pub fn resolve_chat(&self, chat_id: Option<i64>) -> Result<i64> {
        match chat_id {
            Some(id) => Ok(id),
            None => self.hub.default_chat().map_err(|e| anyhow::anyhow!(e)),
        }
    }

    /// Turns a caller-supplied target into a Telegram `message_id`, accepting
    /// either the `#seq` handle transcripts show or a raw `message_id`.
    ///
    /// A `seq` that is not in the log is an error rather than a silent no-op:
    /// it usually means the agent invented a number or is using a stale one,
    /// and quietly dropping the reply would hide that.
    pub fn resolve_target(&self, seq: Option<u64>, message_id: Option<i64>) -> Result<Option<i64>> {
        match (seq, message_id) {
            (Some(seq), _) => self.hub.message_id_for_seq(seq).map(Some).with_context(|| {
                format!(
                    "no message #{seq} in the conversation log — check the #seq handles in \
                     get_conversation, which is also where anything older than the cache is \
                     already gone"
                )
            }),
            (None, id) => Ok(id),
        }
    }

    pub async fn wait_for_reply(
        &self,
        chat_id: Option<i64>,
        after_seq: Option<u64>,
        exclude_own_messages: bool,
        only_addressed: bool,
        only_from_humans: bool,
        timeout_secs: u64,
    ) -> Result<Vec<SimpleMessage>> {
        // Blocking is what sustains an agent↔agent loop: an agent that cannot
        // wait has to end its turn, which ends the cycle. So the brake goes
        // here rather than on sending, where refusing would leave the other
        // side still parked in a wait.
        if self.hub.is_bot_loop(chat_id) {
            anyhow::bail!(
                "the last {} messages in this chat were all from bots, with no person in \
                 between. Refusing to wait for more, because this is how two agents talk each \
                 other in circles indefinitely. End your turn, or send one message summarizing \
                 where things stand and what you need a human to decide. Waiting is allowed \
                 again as soon as a person says something.",
                super::hub::MAX_CONSECUTIVE_BOT_MESSAGES
            );
        }

        let viewer = self.viewer().await;
        // Both of these filters are defined relative to *this* bot, so without
        // an identity they would not merely be approximate — they would
        // silently invert. `exclude_own_messages` would stop excluding
        // anything, letting an agent wake on its own message and reply to
        // itself. Failing loudly beats honouring the opposite of the request.
        if viewer.bot_id.is_none() && (exclude_own_messages || only_addressed) {
            anyhow::bail!(
                "this bot's own identity is unknown (getMe has not succeeded — check the token \
                 and network), so `exclude_own_messages` and `only_addressed` cannot be applied. \
                 Retry, or set both to false to wait on all traffic in the chat."
            );
        }
        let filters = WaitFilters {
            chat_id,
            after_seq,
            // "My own messages" means this agent's bot, not any bot — other
            // agents in this process are exactly who we want to hear from.
            exclude_bot_id: exclude_own_messages.then_some(viewer.bot_id).flatten(),
            only_addressed,
            only_from_humans,
        };
        Ok(self
            .hub
            .wait_for_reply(filters, timeout_secs, &viewer)
            .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_are_normalized_and_url_safe() {
        assert_eq!(AgentId::parse("Qwen").unwrap().to_string(), "qwen");
        assert_eq!(
            AgentId::parse("  opencode ").unwrap().to_string(),
            "opencode"
        );
        assert_eq!(
            AgentId::parse("claude-5_x").unwrap().to_string(),
            "claude-5_x"
        );
    }

    #[test]
    fn agent_names_reject_empty_and_unsafe_values() {
        assert!(AgentId::parse("").is_err());
        assert!(AgentId::parse("   ").is_err());
        // Would break the ?agent= query and the /mcp/{agent} route.
        assert!(AgentId::parse("a b").is_err());
        assert!(AgentId::parse("a/b").is_err());
        assert!(AgentId::parse("a?b").is_err());
    }
}
