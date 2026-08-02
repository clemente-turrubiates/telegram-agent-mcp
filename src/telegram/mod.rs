//! Telegram Bot API access, split so that one process can serve several
//! agents at once.
//!
//! The split exists because of a hard Bot API limitation: Telegram never
//! delivers a message authored by one bot to another bot's `getUpdates`, no
//! matter how the bots or the group are configured. So agent→agent messages
//! cannot travel through Telegram. Instead:
//!
//! - [`hub::Hub`] holds all conversation state, shared by every agent in the
//!   process. Messages an agent sends are written here directly, which is how
//!   other agents see them.
//! - [`session::AgentSession`] is one agent: its bot token, its identity, and
//!   its poller. Only this layer knows "who am I".
//! - [`view`] applies the flags that depend on who is asking (`is_self`,
//!   `addressed_to_me`), which cannot be stored on a shared log.
//! - [`api`] is the wire: one token, retries, error mapping.
//! - [`model`] is the data, all of it viewer-neutral.

pub mod api;
pub mod hub;
pub mod model;
pub mod session;
pub mod view;

pub use hub::Hub;
pub use model::{AgentSelfProfile, SimpleMessage};
pub use session::{AgentId, AgentSession};

/// Locks a mutex, recovering the guard instead of propagating poisoning.
///
/// Everything behind these mutexes is a cache of observed Telegram traffic,
/// not a structure a panic could leave half-updated in a way that matters. The
/// blast radius is what changed: with one process per bot a poisoned lock took
/// down one agent, but the hub is shared, so propagating it would take down
/// every agent — permanently, since a poisoned mutex never recovers on its
/// own. Carrying on with slightly suspect cache state is strictly better.
pub(crate) fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::model::{SimpleMessage, extract_mentions, find_profile_json};

    /// A plain human message, used across the module's tests.
    pub(crate) fn msg(seq: u64, message_id: i64, from: &str, text: &str) -> SimpleMessage {
        SimpleMessage {
            seq,
            message_id,
            chat_id: 1,
            date: 1_700_000_000,
            from_id: Some(42),
            from: Some(from.to_string()),
            from_username: Some(from.to_string()),
            from_is_bot: false,
            from_is_human: true,
            edit_date: None,
            is_announcement: find_profile_json(text).is_some(),
            mentions: extract_mentions(text),
            reply_to_message_id: None,
            text: Some(text.to_string()),
        }
    }

    #[test]
    fn announcements_are_flagged_and_filterable() {
        let plain = msg(1, 1, "alice", "hello");
        let profile = msg(2, 2, "agent_a", "[[AGENT_PROFILE]] {\"model\":\"x\"}");
        assert!(!plain.is_announcement);
        assert!(profile.is_announcement);
    }
}
