//! Data types: what the Telegram API sends us, and what we hand to agents.
//!
//! Everything here is viewer-neutral. Flags that depend on *which* agent is
//! asking (`is_self`, `addressed_to_me`) live in [`super::view`] and are
//! computed at render time, because one log is shared by several agents.

use serde::{Deserialize, Serialize};

/// Prefix marking a chat message as a machine-readable agent profile
/// announcement rather than ordinary conversation text.
pub const PROFILE_MARKER: &str = "[[AGENT_PROFILE]] ";

/// Finds the agent-profile JSON embedded in `text`, if any. The marker may be
/// the whole message, or sit on its own line after a human-readable summary —
/// `announce` sends the latter, so a person reading the chat directly sees
/// something legible instead of a raw JSON blob, while this still finds the
/// payload for `list_agents` to parse.
pub(crate) fn find_profile_json(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix(PROFILE_MARKER))
        .map(str::trim)
}
/// Telegram's hard limit is 4096 UTF-16 code units; stay comfortably under it.
pub const MESSAGE_CHUNK_LEN: usize = 4000;
/// How much of a replied-to message to quote when showing reply context.
pub const REPLY_EXCERPT_LEN: usize = 80;

/// Operator-configured defaults for one agent's announced profile. Any field
/// left `None` here can still be supplied per-call to `announce`.
#[derive(Debug, Clone, Default)]
pub struct AgentSelfProfile {
    pub name: Option<String>,
    pub model: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProfilePayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnownAgent {
    pub bot_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub chat_id: i64,
    /// Unix timestamp (seconds) of the most recent announcement seen.
    pub last_seen: i64,
}

/// Someone observed speaking in a chat. Assembled from message traffic rather
/// than fetched, because bots cannot enumerate a group's membership.
#[derive(Debug, Clone, Serialize)]
pub struct Participant {
    pub user_id: i64,
    /// Display name: the @username if they have one, otherwise their first name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The actual @username, absent for people who never set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub is_bot: bool,
    /// A person rather than a bot. These are the ones to defer to for
    /// direction, and to answer when they ask something.
    #[serde(skip_serializing_if = "is_false")]
    pub is_human: bool,
    /// Whether `mention` can reach them. People who never set a Telegram
    /// username cannot be @mentioned; address them by name in the text
    /// instead, or reply to one of their messages.
    #[serde(skip_serializing_if = "is_false")]
    pub mentionable: bool,
    pub message_count: u64,
    pub last_seen: i64,
}

/// `skip_serializing_if` predicate: a `false` flag carries no information that
/// its absence does not, and these appear on every message and participant.
#[allow(clippy::trivially_copy_pass_by_ref)]
pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatSummary {
    pub id: i64,
    #[serde(rename = "type")]
    pub chat_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    /// Group/channel description. Only returned by getChat, not by updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Text of the chat's pinned message, if any — groups often pin their
    /// purpose or ground rules, which is useful orchestration context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_message: Option<String>,
}

impl ChatSummary {
    /// Legacy basic groups number messages per-bot rather than sharing one id
    /// space, which defeats cross-poller deduplication. Supergroups and
    /// channels do not.
    pub fn is_legacy_basic_group(&self) -> bool {
        self.chat_type == "group"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BotIdentity {
    pub id: i64,
    pub username: Option<String>,
    pub first_name: String,
}

/// Identity of an ingested update, used to drop the duplicate copies that
/// arrive when several bots poll the same chat.
///
/// `edit_date` is part of the key on purpose: an edit of an already-seen
/// message is genuinely new content, so it must not collide with the
/// original, while the *same* edit seen by N pollers still collapses to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DedupKey {
    pub chat_id: i64,
    pub message_id: i64,
    pub edit_date: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimpleMessage {
    /// Monotonically increasing sequence number assigned by this server as
    /// messages are observed/sent, unique across all chats. Use as the
    /// `after_seq` cursor for wait_for_reply / paging.
    pub seq: u64,
    pub message_id: i64,
    pub chat_id: i64,
    pub date: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_id: Option<i64>,
    /// Display name: the sender's @username if set, otherwise their first name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The sender's actual @username, absent if they never set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_username: Option<String>,
    /// The sender is any bot (not necessarily the one asking).
    #[serde(skip_serializing_if = "is_false")]
    pub from_is_bot: bool,
    /// The sender is a person, not a bot.
    #[serde(skip_serializing_if = "is_false")]
    pub from_is_human: bool,
    /// Set when this update was an edit of an earlier message rather than a
    /// new one. Part of an ingested update's identity: the same message edited
    /// twice is two distinct entries, not a duplicate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_date: Option<i64>,
    /// This is a machine-readable agent profile announcement, not conversation.
    #[serde(skip_serializing_if = "is_false")]
    pub is_announcement: bool,
    /// Every @username tagged in the text, so an agent can tell whether a
    /// request was aimed at it, at someone else, or at nobody in particular.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl SimpleMessage {
    pub fn dedup_key(&self) -> DedupKey {
        DedupKey {
            chat_id: self.chat_id,
            message_id: self.message_id,
            edit_date: self.edit_date.unwrap_or(0),
        }
    }
}

/// What a message was replying to, resolved against the cached log so an
/// agent can follow a threaded exchange without extra lookups.
#[derive(Debug, Clone, Serialize)]
pub struct ReplyContext {
    pub message_id: i64,
    /// The target's `#seq` handle, when the target is still in the log. Absent
    /// for a message that predates the cache or has aged out of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationMessage {
    pub seq: u64,
    pub message_id: i64,
    /// How long ago, e.g. `4m`. See [`format_age`].
    pub age: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub from_is_bot: bool,
    /// The sender is a person. Humans usually set the direction here, so
    /// their questions and instructions are the ones worth answering.
    #[serde(skip_serializing_if = "is_false")]
    pub from_is_human: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub is_self: bool,
    /// The message @mentions this bot, or replies to something it said.
    #[serde(skip_serializing_if = "is_false")]
    pub addressed_to_me: bool,
    /// Everyone tagged in the message. If this is non-empty and does not
    /// include you, the request was aimed at someone else — usually a reason
    /// to stay out of it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A chat rendered for an LLM to reason about: who is present, what was said,
/// and where the conversation currently stands.
///
/// `transcript` is the primary output and carries everything an agent needs,
/// including the `#seq` handles used to reply and react. `messages` repeats
/// the same content as structured JSON and is therefore omitted unless asked
/// for — returning both roughly tripled the cost of every read for no added
/// information.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationView {
    pub transcript: String,
    /// Pass to `wait_for_reply`'s `after_seq` to resume exactly here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_seq: Option<u64>,
    /// Set only when older messages exist above the returned window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub older_cached: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<ConversationMessage>>,
    /// Announced agent profiles, included only when a profile carries detail
    /// the roster line cannot (a model name or a description).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<KnownAgent>,
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct ApiResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    pub error_code: Option<i64>,
    pub description: Option<String>,
    pub parameters: Option<ResponseParameters>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponseParameters {
    pub retry_after: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawUser {
    pub id: i64,
    pub first_name: String,
    pub username: Option<String>,
    #[serde(default)]
    pub is_bot: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub chat_type: String,
    pub title: Option<String>,
    pub username: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub description: Option<String>,
    pub pinned_message: Option<Box<RawMessage>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawMessage {
    pub message_id: i64,
    pub date: i64,
    /// Present only on `edited_message` updates.
    pub edit_date: Option<i64>,
    pub chat: RawChat,
    pub from: Option<RawUser>,
    pub text: Option<String>,
    pub caption: Option<String>,
    pub reply_to_message: Option<Box<RawMessage>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawUpdate {
    pub update_id: i64,
    pub message: Option<RawMessage>,
    pub edited_message: Option<RawMessage>,
    pub channel_post: Option<RawMessage>,
}

/// Converts a raw update into a stored message. Deliberately takes no
/// identity: nothing about ingest depends on which bot observed the message,
/// so one shared log can serve several agents.
pub(crate) fn to_simple_message(msg: RawMessage) -> SimpleMessage {
    let from_id = msg.from.as_ref().map(|u| u.id);
    let from_is_bot = msg.from.as_ref().map(|u| u.is_bot).unwrap_or(false);
    let from_username = msg.from.as_ref().and_then(|u| u.username.clone());
    let text = msg.text.or(msg.caption);
    SimpleMessage {
        seq: 0,
        message_id: msg.message_id,
        chat_id: msg.chat.id,
        date: msg.date,
        from_id,
        from: msg
            .from
            .as_ref()
            .map(|u| u.username.clone().unwrap_or_else(|| u.first_name.clone())),
        from_username,
        from_is_bot,
        from_is_human: msg.from.is_some() && !from_is_bot,
        edit_date: msg.edit_date,
        is_announcement: text
            .as_deref()
            .is_some_and(|t| find_profile_json(t).is_some()),
        mentions: text.as_deref().map(extract_mentions).unwrap_or_default(),
        reply_to_message_id: msg.reply_to_message.map(|m| m.message_id),
        text,
    }
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// How long ago `unix_secs` was, as a two-or-three character age like `4m` or
/// `2h`. An absolute UTC timestamp costs ~24 characters on every single line
/// of a transcript and tells a model less than "how stale is this" does — the
/// question an agent actually has is whether a message is still live.
pub(crate) fn format_age(unix_secs: i64, now: i64) -> String {
    let secs = (now - unix_secs).max(0);
    match secs {
        s if s < 60 => "now".to_string(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn excerpt(text: &str, max_chars: usize) -> String {
    let cleaned = text.replace('\n', " ");
    if cleaned.chars().count() <= max_chars {
        return cleaned;
    }
    let cut: String = cleaned.chars().take(max_chars).collect();
    format!("{cut}…")
}

/// Every @username tagged in `text`, in order, without the leading `@` and
/// without duplicates. Telegram usernames are alphanumerics and underscores.
pub(crate) fn extract_mentions(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '@' {
            i += 1;
            continue;
        }
        // A mention starts at the beginning or after a non-word character,
        // so an email address like `a@b` is not treated as tagging `b`.
        let preceded_ok = i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        let mut j = i + 1;
        while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        if preceded_ok && j > i + 1 {
            let name: String = chars[i + 1..j].iter().collect();
            if !found.iter().any(|f| f.eq_ignore_ascii_case(&name)) {
                found.push(name);
            }
        }
        i = j.max(i + 1);
    }
    found
}

/// Splits `text` into chunks that each fit under Telegram's message length
/// limit, preferring to break on newlines.
pub(crate) fn split_text(text: &str) -> Vec<String> {
    if text.chars().count() <= MESSAGE_CHUNK_LEN {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if !current.is_empty() && current.chars().count() + line.chars().count() > MESSAGE_CHUNK_LEN
        {
            chunks.push(std::mem::take(&mut current));
        }
        let mut rest = line;
        while rest.chars().count() > MESSAGE_CHUNK_LEN {
            let split_at = rest
                .char_indices()
                .nth(MESSAGE_CHUNK_LEN)
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            chunks.push(rest[..split_at].to_string());
            rest = &rest[split_at..];
        }
        current.push_str(rest);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_marker_is_found_as_the_whole_message() {
        assert_eq!(
            find_profile_json("[[AGENT_PROFILE]] {\"model\":\"x\"}"),
            Some("{\"model\":\"x\"}")
        );
    }

    #[test]
    fn the_profile_marker_is_found_after_a_human_readable_summary_line() {
        assert_eq!(
            find_profile_json(
                "🤖 planner joined the chat — claude-opus-5\n[[AGENT_PROFILE]] {\"model\":\"x\"}"
            ),
            Some("{\"model\":\"x\"}")
        );
    }

    #[test]
    fn ordinary_text_has_no_profile_json() {
        assert_eq!(find_profile_json("just chatting, no marker here"), None);
    }

    fn mentions_username(text: &str, username: &str) -> bool {
        extract_mentions(text)
            .iter()
            .any(|m| m.eq_ignore_ascii_case(username))
    }

    #[test]
    fn mention_requires_a_word_boundary() {
        assert!(mentions_username("hey @agent_a can you look", "agent_a"));
        assert!(mentions_username("@AGENT_A ping", "agent_a"));
        assert!(mentions_username("cc @agent_a, thanks", "agent_a"));
        // Must not match a longer username that merely starts the same way.
        assert!(!mentions_username("hey @agent_alpha", "agent_a"));
        assert!(!mentions_username("no mention here", "agent_a"));
    }

    #[test]
    fn extract_mentions_finds_everyone_tagged() {
        assert_eq!(
            extract_mentions("@alice and @bob_2, please look"),
            vec!["alice", "bob_2"]
        );
        // Deduplicated, case-insensitively.
        assert_eq!(extract_mentions("@alice @ALICE"), vec!["alice"]);
        // An email address does not tag its domain.
        assert_eq!(
            extract_mentions("mail me at bob@example.com"),
            Vec::<String>::new()
        );
        // A bare @ is not a mention.
        assert_eq!(extract_mentions("meet @ 5pm"), Vec::<String>::new());
        assert_eq!(extract_mentions("no tags here"), Vec::<String>::new());
    }

    #[test]
    fn excerpt_truncates_and_flattens_newlines() {
        assert_eq!(excerpt("one\ntwo", 20), "one two");
        assert_eq!(excerpt("abcdefghij", 4), "abcd…");
    }

    #[test]
    fn split_text_keeps_chunks_within_the_limit() {
        let long = "x".repeat(MESSAGE_CHUNK_LEN * 2 + 100);
        let chunks = split_text(&long);
        assert!(chunks.len() >= 3);
        assert!(
            chunks
                .iter()
                .all(|c| c.chars().count() <= MESSAGE_CHUNK_LEN)
        );
        assert_eq!(chunks.concat(), long);
    }

    #[test]
    fn split_text_leaves_short_text_alone() {
        assert_eq!(split_text("hello"), vec!["hello"]);
    }

    #[test]
    fn an_edit_does_not_collide_with_the_message_it_edits() {
        let mut original = super::super::tests::msg(1, 10, "alice", "hello");
        let mut edited = original.clone();
        edited.edit_date = Some(1_700_000_500);
        assert_ne!(original.dedup_key(), edited.dedup_key());
        // The same edit seen twice is still one message.
        original.edit_date = Some(1_700_000_500);
        assert_eq!(original.dedup_key(), edited.dedup_key());
    }
}
