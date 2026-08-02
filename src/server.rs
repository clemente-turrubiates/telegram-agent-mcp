use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Extensions, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, schemars, tool, tool_handler, tool_router};

use crate::flexible_id::{
    deserialize_i64, deserialize_opt_i64, deserialize_opt_u64, i64_schema, u64_schema,
};
use crate::registry::AgentRegistry;
use crate::telegram::AgentSession;

const MAX_WAIT_SECS: u64 = 120;

/// Resolves an omitted `chat_id` to the only chat the server knows about.
fn resolve_chat(telegram: &Arc<AgentSession>, chat_id: Option<i64>) -> Result<i64, McpError> {
    telegram
        .resolve_chat(chat_id)
        .map_err(|err| McpError::invalid_params(err.to_string(), None))
}

/// Confirms a send in one line rather than echoing the whole message back.
///
/// The caller already knows what it sent; the only genuinely new facts are
/// the `#seq` handles it can now be replied to or reacted to by, and whether
/// long text got split. Returning the full serialized message instead cost
/// several hundred tokens per send to tell an agent what it just wrote.
fn describe_sent(sent: &[crate::telegram::SimpleMessage]) -> String {
    let seqs: Vec<String> = sent.iter().map(|m| format!("#{}", m.seq)).collect();
    match seqs.len() {
        0 => "sent (nothing recorded)".to_string(),
        1 => format!("sent as {}", seqs[0]),
        n => format!("sent as {} ({n} parts, split for length)", seqs.join(" ")),
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendMessageRequest {
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    #[schemars(
        description = "Chat to send to. Omit it when there is only one chat, which is the usual case — the server fills it in.",
        schema_with = "i64_schema"
    )]
    pub chat_id: Option<i64>,
    #[schemars(description = "Message text to send")]
    pub text: String,
    #[serde(default, deserialize_with = "deserialize_opt_u64")]
    #[schemars(
        description = "Optional #seq of the message you are replying to, exactly as shown in the transcript (pass 3 for `#3`). Threads your message under it.",
        schema_with = "u64_schema"
    )]
    pub reply_to_seq: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    #[schemars(
        description = "Raw Telegram message_id to reply to. Prefer reply_to_seq; this exists for callers that already hold a message_id.",
        schema_with = "i64_schema"
    )]
    pub reply_to_message_id: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetConversationRequest {
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    #[schemars(
        description = "Chat to read. Omit it when there is only one chat, which is the usual case — the server fills it in.",
        schema_with = "i64_schema"
    )]
    pub chat_id: Option<i64>,
    #[schemars(description = "How many of the most recent messages to include (default 50)")]
    pub limit: Option<u32>,
    #[schemars(
        description = "Include machine-readable agent profile announcements in the transcript, hidden by default (default false)"
    )]
    pub include_announcements: Option<bool>,
    #[schemars(
        description = "Also return the messages as a structured JSON array. Off by default: the transcript already carries everything, including the #seq handles, and returning both roughly triples the size of the result for no extra information. Turn on only if you specifically need to process fields programmatically."
    )]
    pub include_json: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchMessagesRequest {
    #[schemars(description = "Text to search for, case-insensitive substring match")]
    pub query: String,
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    #[schemars(
        description = "If set, only search within this chat ID",
        schema_with = "i64_schema"
    )]
    pub chat_id: Option<i64>,
    #[schemars(description = "Maximum number of matches to return (default 30)")]
    pub limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetChatRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    #[schemars(description = "Chat ID to look up", schema_with = "i64_schema")]
    pub chat_id: i64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WaitForReplyRequest {
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    #[schemars(
        description = "If set, only wait for messages in this chat ID",
        schema_with = "i64_schema"
    )]
    pub chat_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_u64")]
    #[schemars(
        description = "Only return messages with seq greater than this (use the `seq` field from a previous message/tool result as a cursor). If omitted, waits only for messages that arrive after this call starts.",
        schema_with = "u64_schema"
    )]
    pub after_seq: Option<u64>,
    #[schemars(
        description = "If true (default), ignore this bot's own messages so you only wake up on messages from others"
    )]
    pub exclude_own_messages: Option<bool>,
    #[schemars(
        description = "If true, only wake for messages that @mention you or reply to something you said, ignoring unrelated chatter (default false)"
    )]
    pub only_addressed: Option<bool>,
    #[schemars(
        description = "If true, only wake for messages from human users, ignoring other bots. Use after asking the person a question, so agent chatter doesn't count as their answer (default false)."
    )]
    pub only_from_humans: Option<bool>,
    #[schemars(description = "Max seconds to wait, capped at 120 (default 60)")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AnnounceRequest {
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    #[schemars(
        description = "Chat to announce in. Omit when there is only one chat.",
        schema_with = "i64_schema"
    )]
    pub chat_id: Option<i64>,
    #[schemars(
        description = "Display name to announce yourself as. Falls back to a server-configured default, then your Telegram username."
    )]
    pub name: Option<String>,
    #[schemars(
        description = "Your own model identity, e.g. \"claude-opus-5\", \"gpt-5\", \"gemini-3-pro\" — state whatever model you actually are, so other agents can decide whether you're a good fit for a task."
    )]
    pub model: Option<String>,
    #[schemars(
        description = "Free-text description of your strengths/specialties, e.g. \"good at Rust systems programming and code review\""
    )]
    pub description: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MentionRequest {
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    #[schemars(
        description = "Chat to send in. Omit when there is only one chat.",
        schema_with = "i64_schema"
    )]
    pub chat_id: Option<i64>,
    #[schemars(
        description = "Telegram usernames (without @) to tag — agents or humans alike. Take them from the roster at the top of get_conversation. Someone who never set a username cannot be tagged at all; the roster says so, and the thing to do is reply to one of their messages instead."
    )]
    pub usernames: Vec<String>,
    #[schemars(description = "Message text explaining what you want from them")]
    pub text: String,
    #[serde(default, deserialize_with = "deserialize_opt_u64")]
    #[schemars(
        description = "Optional #seq of the message this responds to, as shown in the transcript (pass 3 for `#3`). Keeps the handoff threaded under the request it answers.",
        schema_with = "u64_schema"
    )]
    pub reply_to_seq: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    #[schemars(
        description = "Raw Telegram message_id to reply to. Prefer reply_to_seq.",
        schema_with = "i64_schema"
    )]
    pub reply_to_message_id: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReactRequest {
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    #[schemars(
        description = "Chat the message is in. Omit when there is only one chat.",
        schema_with = "i64_schema"
    )]
    pub chat_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_u64")]
    #[schemars(
        description = "#seq of the message to react to, as shown in the transcript (pass 3 for `#3`).",
        schema_with = "u64_schema"
    )]
    pub seq: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    #[schemars(
        description = "Raw Telegram message_id to react to. Prefer seq.",
        schema_with = "i64_schema"
    )]
    pub message_id: Option<i64>,
    #[schemars(
        description = "One emoji from Telegram's fixed reaction set, e.g. \"👍\", \"👀\", \"✅\", \"🔥\", \"🎉\", \"❤\". An emoji outside that set is rejected by Telegram with an error. Omit to clear this bot's own reaction on the message."
    )]
    pub emoji: Option<String>,
}

/// One MCP connection. Every tool acts as exactly one agent; which one is
/// either fixed when the connection is created (stdio, or a `/mcp/<agent>`
/// route) or read from `?agent=` on each request.
#[derive(Clone)]
pub struct TelegramMcpServer {
    bound: Option<Arc<AgentSession>>,
    registry: Option<Arc<AgentRegistry>>,
}

#[tool_router]
impl TelegramMcpServer {
    /// A connection that always acts as one agent.
    pub fn bound(session: Arc<AgentSession>) -> Self {
        Self {
            bound: Some(session),
            registry: None,
        }
    }

    /// A connection that resolves its agent per request.
    pub fn unbound(registry: Arc<AgentRegistry>) -> Self {
        Self {
            bound: None,
            registry: Some(registry),
        }
    }

    /// Which agent this call acts as.
    ///
    /// Resolution is per request rather than captured at `initialize`, because
    /// a client negotiating a stateless protocol version never sends one — its
    /// requests each build a fresh handler. Reading the query string every
    /// time works in both modes.
    fn agent(&self, extensions: &Extensions) -> Result<Arc<AgentSession>, McpError> {
        if let Some(session) = &self.bound {
            return Ok(Arc::clone(session));
        }
        let registry = self.registry.as_ref().ok_or_else(|| {
            McpError::internal_error("server has no agents configured".to_string(), None)
        })?;

        let requested = extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.uri.query())
            .and_then(crate::http::agent_from_query);

        match requested {
            Some(name) => registry.get(name).ok_or_else(|| {
                McpError::invalid_params(
                    format!("unknown agent {name:?}. {}", registry.selection_help()),
                    None,
                )
            }),
            None => registry
                .default_session()
                .ok_or_else(|| McpError::invalid_params(registry.selection_help(), None)),
        }
    }

    #[tool(
        description = "Check which Telegram bot you are speaking as. Rarely needed — get_conversation already marks your own messages `you` and your roster entry `(you)`."
    )]
    async fn whoami(&self, extensions: Extensions) -> Result<String, McpError> {
        let telegram = self.agent(&extensions)?;
        let me = telegram
            .get_me()
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        Ok(serde_json::to_string(&me).unwrap_or_default())
    }

    #[tool(
        description = "Introduce yourself, once per conversation, so other agents know who you are and what to send you. State the model you actually are — the roster shows it to everyone deciding who should take a task. This is metadata, not conversation: it stays out of the transcript and needs no reply."
    )]
    async fn announce(
        &self,
        Parameters(AnnounceRequest {
            chat_id,
            name,
            model,
            description,
        }): Parameters<AnnounceRequest>,
        extensions: Extensions,
    ) -> Result<String, McpError> {
        let telegram = self.agent(&extensions)?;
        let chat_id = resolve_chat(&telegram, chat_id)?;
        let sent = telegram
            .announce(chat_id, name, model, description)
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        Ok(describe_sent(&sent))
    }

    #[tool(
        description = "Look up which agents exist and what they are good at, when deciding who should take a task. get_conversation already shows this in its roster for the current chat, so you rarely need this separately."
    )]
    async fn list_agents(&self, extensions: Extensions) -> Result<String, McpError> {
        let agents = self.agent(&extensions)?.list_known_agents();
        Ok(serde_json::to_string(&agents).unwrap_or_default())
    }

    #[tool(
        description = "Like send_message, but @tags specific people so they are notified — use it to hand work to a better-suited agent, or to ask a person a question. Take usernames from the roster at the top of get_conversation. Check that roster first: tagging an agent marked `idle` is not a handoff, because nothing is running to receive it. Follow with wait_for_reply if you expect an answer."
    )]
    async fn mention(
        &self,
        Parameters(MentionRequest {
            chat_id,
            usernames,
            text,
            reply_to_seq,
            reply_to_message_id,
        }): Parameters<MentionRequest>,
        extensions: Extensions,
    ) -> Result<String, McpError> {
        let telegram = self.agent(&extensions)?;
        let chat_id = resolve_chat(&telegram, chat_id)?;
        if usernames.is_empty() {
            return Err(McpError::invalid_params(
                "usernames must not be empty; use send_message to address the chat generally",
                None,
            ));
        }
        let reply_to = telegram
            .resolve_target(reply_to_seq, reply_to_message_id)
            .map_err(|err| McpError::invalid_params(err.to_string(), None))?;
        let tags = usernames
            .iter()
            .map(|u| format!("@{}", u.trim().trim_start_matches('@')))
            .collect::<Vec<_>>()
            .join(" ");
        let message = format!("{tags} {text}");
        let sent = telegram
            .send_message(chat_id, &message, reply_to)
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        Ok(describe_sent(&sent))
    }

    #[tool(
        description = "Put an emoji reaction on a PERSON's message, as a read receipt meaning 'seen, working on it'. Rarely what you want: a reaction is not an answer, so if they asked something, reply with send_message. Does not work on another agent's message — Telegram forbids it — so acknowledge an agent with one short line of text instead. Only Telegram's fixed set works (👍 👀 ✅ 🔥 and similar)."
    )]
    async fn react(
        &self,
        Parameters(ReactRequest {
            chat_id,
            seq,
            message_id,
            emoji,
        }): Parameters<ReactRequest>,
        extensions: Extensions,
    ) -> Result<String, McpError> {
        let telegram = self.agent(&extensions)?;
        let chat_id = resolve_chat(&telegram, chat_id)?;
        let target = telegram
            .resolve_target(seq, message_id)
            .map_err(|err| McpError::invalid_params(err.to_string(), None))?
            .ok_or_else(|| {
                McpError::invalid_params("pass seq (preferred) or message_id to react to", None)
            })?;
        telegram
            .react(chat_id, target, emoji.as_deref())
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        Ok("reacted".to_string())
    }

    #[tool(
        description = "Say something in the chat. Usually all you need is `text` — omit chat_id when there is only one chat. Pass `reply_to_seq` to answer a specific message, which is clearer and cheaper than restating what you are replying to. Keep it to a couple of sentences; this is a chat window, not a report. Over-long text is split automatically. If you expect an answer, call wait_for_reply next rather than ending your turn — once your turn ends nothing on Telegram can wake you."
    )]
    async fn send_message(
        &self,
        Parameters(SendMessageRequest {
            chat_id,
            text,
            reply_to_seq,
            reply_to_message_id,
        }): Parameters<SendMessageRequest>,
        extensions: Extensions,
    ) -> Result<String, McpError> {
        let telegram = self.agent(&extensions)?;
        let chat_id = resolve_chat(&telegram, chat_id)?;
        let reply_to = telegram
            .resolve_target(reply_to_seq, reply_to_message_id)
            .map_err(|err| McpError::invalid_params(err.to_string(), None))?;
        let sent = telegram
            .send_message(chat_id, &text, reply_to)
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        Ok(describe_sent(&sent))
    }

    #[tool(
        description = "List the chats this bot knows about. Only needed when there are several and you must choose — with a single chat, every tool defaults to it. A bot learns of a chat only once it sees traffic there, so this is empty until someone speaks."
    )]
    async fn list_chats(&self, extensions: Extensions) -> Result<String, McpError> {
        let chats = self.agent(&extensions)?.list_known_chats();
        Ok(serde_json::to_string(&chats).unwrap_or_default())
    }

    #[tool(
        description = "Fetch fresh details for one chat, including its description and pinned message. get_conversation already shows these for the chat you are reading, so this is mainly for a different chat."
    )]
    async fn get_chat(
        &self,
        Parameters(GetChatRequest { chat_id }): Parameters<GetChatRequest>,
        extensions: Extensions,
    ) -> Result<String, McpError> {
        let telegram = self.agent(&extensions)?;
        let chat = telegram
            .get_chat(chat_id)
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        Ok(serde_json::to_string(&chat).unwrap_or_default())
    }

    #[tool(
        description = "START HERE — reads the group chat. Call this first, with no arguments, before doing anything else in Telegram. Returns a readable thread: a header naming the chat and everyone in it (agents listed with the model and specialties they announced, and whether each is `listening` or `idle`), then one entry per message as `#seq @author · age` plus badges — `◄ FOR YOU` if it @mentions you or replies to you, `→ @someone` if it was aimed at someone else (then it is theirs to answer, not yours), `↳#N` for what it replies to, `human` for a person. A trailing ⚠ line means a person is waiting on you. Reuse the `#seq` numbers as `reply_to_seq` (send_message, mention), `seq` (react), and `after_seq` (wait_for_reply)."
    )]
    async fn get_conversation(
        &self,
        Parameters(GetConversationRequest {
            chat_id,
            limit,
            include_announcements,
            include_json,
        }): Parameters<GetConversationRequest>,
        extensions: Extensions,
    ) -> Result<String, McpError> {
        let telegram = self.agent(&extensions)?;
        let chat_id = resolve_chat(&telegram, chat_id)?;
        let limit = limit.unwrap_or(50) as usize;
        let view = telegram
            .get_conversation(
                chat_id,
                limit,
                include_announcements.unwrap_or(false),
                include_json.unwrap_or(false),
            )
            .await;

        // The transcript is the product here, so return it as text rather than
        // as a JSON string field: escaping every newline to `\n` costs tokens
        // and makes it markedly harder to read. Only the opt-in structured
        // form goes back as JSON.
        match view.messages {
            None => Ok(view.transcript),
            Some(_) => Ok(serde_json::to_string(&view).unwrap_or_default()),
        }
    }

    #[tool(
        description = "Find earlier messages containing some text, to recall what was said about a topic without re-reading the whole conversation. Returns the same `#seq` thread format as get_conversation."
    )]
    async fn search_messages(
        &self,
        Parameters(SearchMessagesRequest {
            query,
            chat_id,
            limit,
        }): Parameters<SearchMessagesRequest>,
        extensions: Extensions,
    ) -> Result<String, McpError> {
        let telegram = self.agent(&extensions)?;
        let limit = limit.unwrap_or(30) as usize;
        let hits = telegram.search_messages(&query, chat_id, limit);
        if hits.is_empty() {
            return Ok(format!("no cached message matches {query:?}"));
        }
        Ok(telegram.render(&hits).await)
    }

    #[tool(
        description = "Wait for someone to reply. THIS IS THE ONLY WAY TO STAY REACHABLE: while this call is running a message reaches you instantly, and the moment your turn ends nothing in Telegram can wake you again — only the user typing into this CLI can. So after asking a question or handing off work, call this instead of ending your turn. It returns after at most 120s; a timeout means nobody has answered YET, not that you should stop — call it again if you still want the answer. `only_addressed=true` wakes you only when someone tags or replies to you, which is how to idle quietly in a busy group. `only_from_humans=true` waits for a person specifically, so agent chatter is not mistaken for their answer. `after_seq` resumes from a `#seq` you have already seen."
    )]
    async fn wait_for_reply(
        &self,
        Parameters(WaitForReplyRequest {
            chat_id,
            after_seq,
            exclude_own_messages,
            only_addressed,
            only_from_humans,
            timeout_secs,
        }): Parameters<WaitForReplyRequest>,
        extensions: Extensions,
    ) -> Result<String, McpError> {
        let telegram = self.agent(&extensions)?;
        let timeout_secs = timeout_secs.unwrap_or(60).min(MAX_WAIT_SECS);

        let messages = telegram
            .wait_for_reply(
                chat_id,
                after_seq,
                exclude_own_messages.unwrap_or(true),
                only_addressed.unwrap_or(false),
                only_from_humans.unwrap_or(false),
                timeout_secs,
            )
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;

        // Says which it is rather than returning a bare empty result, because
        // "nobody answered yet" and "here is the answer" call for different
        // next moves and an empty JSON array states neither.
        if messages.is_empty() {
            return Ok(format!(
                "nothing new after {timeout_secs}s. If you are still expecting a reply, call \
                 wait_for_reply again rather than ending your turn — once your turn ends nothing \
                 on Telegram can wake you."
            ));
        }
        Ok(telegram.render(&messages).await)
    }
}

#[tool_handler]
impl ServerHandler for TelegramMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "A shared Telegram group chat with people and other LLM agents in it.\n\n\
                 THE LOOP\n\
                 1. `get_conversation` (no arguments) — always do this first. It shows who is \
                 present and what has been said.\n\
                 2. `announce` once, early, so others know which model you are and what to send \
                 you.\n\
                 3. Reply with `send_message`, or `mention` to tag someone specifically.\n\
                 4. `wait_for_reply` — call it instead of ending your turn, and call it again if \
                 it times out.\n\
                 5. Back to 3.\n\n\
                 THE ONE RULE THAT MATTERS\n\
                 You are only reachable while a tool call is running. The moment your turn ends, \
                 nothing that happens in Telegram can wake you — only the user typing into this \
                 CLI can. So if you asked a question, tagged someone, or handed off work, sit in \
                 `wait_for_reply` rather than finishing. A timeout means nobody has answered yet; \
                 call it again.\n\n\
                 READING THE TRANSCRIPT\n\
                 Each line is `#seq @author · age` with badges: `◄ FOR YOU` means it @mentions \
                 you or replies to you — answer it. `→ @someone` means it was aimed at someone \
                 else — stay out of it. `↳#N` is what it replies to. `human` marks a person. A \
                 trailing `⚠` line means a person is waiting on you; answer them before \
                 continuing with other agents. Pass any `#seq` back as `reply_to_seq` \
                 (send_message, mention) or `after_seq` (wait_for_reply); you never need \
                 Telegram's own message ids.\n\n\
                 HOW TO WRITE\n\
                 A couple of sentences. This is a chat window, not a report — say what you did, \
                 found, or need, then stop. Answer in words: `react` is a read receipt for a \
                 person, never a reply, and never works on another agent. Taking work from \
                 another agent? Say so in one line. Finished it? Say what came of it.\n\n\
                 WORKING WITH THE OTHERS\n\
                 The roster marks each agent `listening` or `idle`. Only a `listening` agent will \
                 receive what you send it right now; tagging an `idle` one is not a handoff, \
                 because nothing is running to receive it — do the work yourself or ask a person. \
                 People are why this chat exists: treat their instructions as direction, and use \
                 `wait_for_reply` with `only_from_humans=true` when you need an answer from one \
                 specifically. Anyone without a username cannot be tagged; reply to their message \
                 instead.\n\n\
                 LIMITS\n\
                 Bots see only chats they were added to, and see every message there only if \
                 privacy mode is off in @BotFather. There is no history before this server \
                 started — the Bot API has no endpoint for it."
                    .to_string(),
            )
    }
}
