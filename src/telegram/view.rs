//! Viewer-relative rendering.
//!
//! Several agents share one message log, so "is this mine?" and "was this
//! aimed at me?" are not properties of a message — they depend on who is
//! asking. Nothing here is stored; it is all computed per request from a
//! [`Viewer`].

use serde::Serialize;

use super::model::{BotIdentity, ConversationMessage, KnownAgent, Participant, SimpleMessage};

/// Who is looking at the log.
#[derive(Debug, Clone, Default)]
pub struct Viewer {
    pub bot_id: Option<i64>,
    pub username: Option<String>,
}

impl Viewer {
    pub fn new(identity: Option<&BotIdentity>) -> Self {
        Self {
            bot_id: identity.map(|i| i.id),
            username: identity.and_then(|i| i.username.clone()),
        }
    }

    /// Whether `from_id` is this viewer. A viewer with no known bot id never
    /// matches, rather than matching every anonymous sender.
    pub fn is_self(&self, from_id: Option<i64>) -> bool {
        self.bot_id.is_some() && from_id == self.bot_id
    }

    fn is_mentioned_in(&self, mentions: &[String]) -> bool {
        self.username
            .as_deref()
            .is_some_and(|u| mentions.iter().any(|m| m.eq_ignore_ascii_case(u)))
    }
}

/// A participant plus the viewer-relative flags that are not stored on them.
#[derive(Debug, Clone, Serialize)]
pub struct ParticipantView {
    #[serde(flatten)]
    pub participant: Participant,
    /// True for the bot the asking agent is running as.
    pub is_self: bool,
}

impl ParticipantView {
    pub fn of(participant: Participant, viewer: &Viewer) -> Self {
        Self {
            is_self: viewer.is_self(Some(participant.user_id)),
            participant,
        }
    }
}

/// Whether `msg` was aimed at `viewer` — either by @mentioning their username
/// or by replying to something they said. `reply_author` resolves a
/// `message_id` to the user id that sent it.
///
/// A viewer's own message is never "addressed to" them, so an agent does not
/// treat its own @mention of itself as an inbound request.
pub fn is_addressed_to(
    msg: &SimpleMessage,
    viewer: &Viewer,
    reply_author: impl Fn(i64) -> Option<i64>,
) -> bool {
    if viewer.is_self(msg.from_id) {
        return false;
    }
    if viewer.is_mentioned_in(&msg.mentions) {
        return true;
    }
    msg.reply_to_message_id
        .and_then(reply_author)
        .is_some_and(|author| viewer.is_self(Some(author)))
}

/// Renders a chat the way a code-review thread reads: a header saying where
/// you are and who is present, then one entry per message carrying its own
/// `#seq` handle, author, age, and what it responds to.
///
/// Everything an agent needs to act is on the entry line, so the structured
/// `messages` array is redundant and is not returned by default. `#seq` is
/// the single handle used everywhere — `reply_to_seq`, `react`, `after_seq` —
/// rather than making agents carry a second Telegram `message_id` around.
pub(crate) fn render_transcript(
    messages: &[ConversationMessage],
    header: Option<&TranscriptHeader<'_>>,
) -> String {
    let mut out = String::new();

    if let Some(h) = header {
        out.push_str(&h.render());
        out.push('\n');
    }

    if messages.is_empty() {
        out.push_str("(no messages yet)");
        return out;
    }

    for m in messages {
        let who = match (m.is_self, m.from.as_deref()) {
            (true, _) => "you".to_string(),
            (false, Some(name)) => format!("@{name}"),
            (false, None) => "unknown".to_string(),
        };

        // Badges, in the order an agent should care about them: is this mine
        // to answer, whose turn is it otherwise, what does it follow.
        let mut badges: Vec<String> = Vec::new();
        if m.addressed_to_me {
            badges.push("◄ FOR YOU".to_string());
        } else if !m.mentions.is_empty() {
            badges.push(format!("→ @{}", m.mentions.join(" @")));
        }
        if let Some(r) = &m.reply_to {
            badges.push(match r.seq {
                Some(seq) => format!("↳#{seq}"),
                None => "↳(older)".to_string(),
            });
        }
        if m.from_is_human {
            badges.push("human".to_string());
        }

        out.push_str(&format!("#{} {} · {}", m.seq, who, m.age));
        if !badges.is_empty() {
            out.push_str(&format!(" · {}", badges.join(" · ")));
        }
        out.push('\n');

        for line in m.text.as_deref().unwrap_or("(no text)").lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }

    if let Some(note) = pending_human_reply(messages) {
        out.push('\n');
        out.push_str(&note);
        out.push('\n');
    }

    out.trim_end().to_string()
}

/// Flags the last thing a person said if this agent has not spoken since.
///
/// Agents are good at talking to each other and bad at noticing they left a
/// person hanging — the human's message scrolls up, the agents carry on, and
/// nobody answers the one participant who was actually asking. Stating it
/// outright costs a line and is the difference between a conversation and two
/// bots ignoring someone.
fn pending_human_reply(messages: &[ConversationMessage]) -> Option<String> {
    let last_human = messages.iter().rposition(|m| m.from_is_human)?;
    let answered = messages[last_human + 1..].iter().any(|m| m.is_self);
    if answered {
        return None;
    }
    let m = &messages[last_human];
    let who = m
        .from
        .as_deref()
        .map(|n| format!("@{n}"))
        .unwrap_or_else(|| "a person".to_string());
    Some(format!(
        "⚠ {who} spoke last at #{} and you have not replied since. If it was aimed at you, \
         answer it before carrying on with other agents.",
        m.seq
    ))
}

/// The context line above a transcript: which chat, who is in it, and how far
/// back the window reaches.
pub(crate) struct TranscriptHeader<'a> {
    pub chat_type: &'a str,
    pub title: Option<&'a str>,
    pub chat_username: Option<&'a str>,
    pub pinned: Option<&'a str>,
    pub participants: &'a [ParticipantView],
    /// Announced profiles, so the roster can say what each agent is good at —
    /// the information you actually route work by.
    pub agents: &'a [KnownAgent],
    /// Bot ids currently blocked in `wait_for_reply`. Everyone else will not
    /// see a new message until something starts a turn for them.
    pub listening: &'a std::collections::HashSet<i64>,
    pub older_cached: usize,
}

impl TranscriptHeader<'_> {
    fn render(&self) -> String {
        let mut out = String::new();

        let title = self.title.unwrap_or("(untitled)");
        out.push_str(&format!("{} · {title}", self.chat_type));
        if let Some(u) = self.chat_username {
            out.push_str(&format!(" @{u}"));
        }
        out.push('\n');

        // One roster line, grouped by what an agent does with each group:
        // humans direct the work, agents share it, and anyone unmentionable
        // has to be answered by reply rather than by tag.
        let mut humans: Vec<String> = Vec::new();
        let mut agents: Vec<String> = Vec::new();
        for p in self.participants {
            let mut label = match (&p.participant.username, &p.participant.name) {
                (Some(u), _) if p.is_self => format!("@{u} (you)"),
                (Some(u), _) => format!("@{u}"),
                (None, Some(n)) => format!("{n} (no username — reply to reach)"),
                (None, None) => continue,
            };
            if p.participant.is_human {
                humans.push(label);
                continue;
            }
            // Whether a message sent now would actually reach them. Not
            // cosmetic: handing work to an agent that is not listening is how
            // a task quietly goes nowhere.
            if !p.is_self {
                label.push_str(if self.listening.contains(&p.participant.user_id) {
                    " (listening)"
                } else {
                    " (idle — will not see this until its next turn)"
                });
            }
            // What this agent said it is good at, which is how another agent
            // decides whether to hand it the work.
            if let Some(a) = self
                .agents
                .iter()
                .find(|a| a.bot_id == p.participant.user_id)
            {
                let skills: Vec<&str> = [a.model.as_deref(), a.description.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect();
                if !skills.is_empty() {
                    label.push_str(&format!(" — {}", skills.join(", ")));
                }
            }
            agents.push(label);
        }
        // Separated by `·` rather than a comma, because a description is free
        // text that frequently contains commas of its own.
        if !humans.is_empty() {
            out.push_str(&format!("humans: {}\n", humans.join(" · ")));
        }
        if !agents.is_empty() {
            out.push_str(&format!("agents: {}\n", agents.join(" · ")));
        }

        if let Some(p) = self.pinned {
            out.push_str(&format!("pinned: {p}\n"));
        }
        if self.older_cached > 0 {
            out.push_str(&format!(
                "({} older message(s) cached above — raise `limit` to see them)\n",
                self.older_cached
            ));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::msg;
    use super::*;
    use crate::telegram::model::ReplyContext;
    use std::collections::HashSet;

    /// A viewer for a bot with id 7 and username `me`.
    fn me_viewer() -> Viewer {
        Viewer {
            bot_id: Some(7),
            username: Some("me".to_string()),
        }
    }

    fn no_replies(_: i64) -> Option<i64> {
        None
    }

    #[test]
    fn a_message_tagging_someone_else_is_not_addressed_to_me() {
        let m = msg(1, 1, "human_user", "@agent_b can you take this one");
        assert!(m.mentions.iter().any(|x| x == "agent_b"));
        assert!(!is_addressed_to(&m, &me_viewer(), no_replies));
    }

    #[test]
    fn a_message_tagging_me_is_addressed_to_me() {
        let m = msg(1, 1, "human_user", "@me can you take this one");
        assert!(is_addressed_to(&m, &me_viewer(), no_replies));
    }

    #[test]
    fn a_reply_to_something_i_said_is_addressed_to_me() {
        let mut m = msg(2, 20, "human_user", "sounds good");
        m.reply_to_message_id = Some(10);
        // Message 10 was sent by bot id 7, i.e. by me.
        assert!(is_addressed_to(&m, &me_viewer(), |rid| (rid == 10).then_some(7)));
        // A reply to somebody else's message is not.
        assert!(!is_addressed_to(&m, &me_viewer(), |rid| (rid == 10).then_some(99)));
    }

    #[test]
    fn my_own_message_is_never_addressed_to_me() {
        let mut m = msg(3, 30, "me", "@me note to self");
        m.from_id = Some(7);
        assert!(!is_addressed_to(&m, &me_viewer(), no_replies));
    }

    #[test]
    fn a_viewer_with_no_identity_matches_nobody() {
        let m = msg(4, 40, "human_user", "@me hello");
        let anonymous = Viewer::default();
        assert!(!is_addressed_to(&m, &anonymous, no_replies));
        // An unidentified viewer must not claim anonymous messages as its own.
        assert!(!anonymous.is_self(None));
    }

    #[test]
    fn is_self_is_relative_to_the_asking_agent() {
        let mut m = msg(5, 50, "agent_a", "hi");
        m.from_id = Some(7);
        let other = Viewer {
            bot_id: Some(8),
            username: Some("other".to_string()),
        };
        assert!(me_viewer().is_self(m.from_id));
        assert!(!other.is_self(m.from_id));
    }

    /// The same stored message must render differently for different agents —
    /// the whole reason these flags are not stored.
    #[test]
    fn one_stored_message_renders_per_viewer() {
        let mut m = msg(6, 60, "agent_a", "@me over to you");
        m.from_id = Some(8);

        let author = Viewer {
            bot_id: Some(8),
            username: Some("agent_a".to_string()),
        };
        assert!(!me_viewer().is_self(m.from_id));
        assert!(author.is_self(m.from_id));
        assert!(is_addressed_to(&m, &me_viewer(), no_replies));
    }

    #[test]
    fn participant_view_serializes_is_self_alongside_participant_fields() {
        let view = ParticipantView {
            is_self: false,
            participant: Participant {
                user_id: 7,
                name: Some("alice".into()),
                username: Some("alice".into()),
                is_bot: false,
                is_human: true,
                mentionable: true,
                message_count: 3,
                last_seen: 1,
            },
        };
        let json: serde_json::Value = serde_json::to_value(&view).unwrap();
        assert_eq!(json["is_self"], serde_json::json!(false));
        assert_eq!(json["user_id"], serde_json::json!(7));
        assert!(json.get("participant").is_none());
    }

    fn convo_msg(seq: u64, from: &str, text: &str) -> ConversationMessage {
        ConversationMessage {
            seq,
            message_id: seq as i64 + 100,
            age: "2m".into(),
            from: Some(from.into()),
            from_is_bot: false,
            from_is_human: true,
            is_self: false,
            addressed_to_me: false,
            mentions: vec![],
            reply_to: None,
            text: Some(text.into()),
        }
    }

    #[test]
    fn transcript_marks_self_and_addressed_messages() {
        let mut inbound = convo_msg(1, "alice", "@me please review");
        inbound.addressed_to_me = true;
        inbound.mentions = vec!["me".into()];

        let mut mine = convo_msg(2, "me", "on it");
        mine.from_is_bot = true;
        mine.from_is_human = false;
        mine.is_self = true;
        mine.reply_to = Some(ReplyContext {
            message_id: 101,
            seq: Some(1),
            from: Some("alice".into()),
            excerpt: Some("@me please review".into()),
        });

        let t = render_transcript(&[inbound, mine], None);
        assert!(t.contains("#1 @alice"), "{t}");
        assert!(t.contains("◄ FOR YOU"), "{t}");
        assert!(t.contains("human"), "{t}");
        // My own message reads as "you", and points back at what it answers.
        assert!(t.contains("#2 you"), "{t}");
        assert!(t.contains("↳#1"), "{t}");
        assert!(t.contains("on it"), "{t}");
    }

    #[test]
    fn transcript_shows_when_someone_else_was_tagged() {
        let mut m = convo_msg(3, "alice", "@agent_b your turn");
        m.mentions = vec!["agent_b".into()];

        let t = render_transcript(&[m], None);
        assert!(t.contains("→ @agent_b"), "{t}");
        // Not aimed at this viewer, so it must not claim to be.
        assert!(!t.contains("FOR YOU"), "{t}");
    }

    #[test]
    fn a_reply_to_something_no_longer_cached_says_so_instead_of_dangling() {
        let mut m = convo_msg(4, "alice", "as I said above");
        m.reply_to = Some(ReplyContext {
            message_id: 55,
            seq: None,
            from: None,
            excerpt: None,
        });
        let t = render_transcript(&[m], None);
        assert!(t.contains("↳(older)"), "{t}");
    }

    #[test]
    fn a_persons_unanswered_message_is_flagged() {
        // alice speaks, then two agents talk to each other and forget her.
        let alice = convo_msg(1, "alice", "which approach are we taking?");
        let mut other_agent = convo_msg(2, "agent_b", "I'd go with the second one");
        other_agent.from_is_bot = true;
        other_agent.from_is_human = false;

        let t = render_transcript(&[alice, other_agent], None);
        assert!(t.contains("⚠ @alice spoke last at #1"), "{t}");
    }

    #[test]
    fn no_warning_once_this_agent_has_replied() {
        let alice = convo_msg(1, "alice", "which approach are we taking?");
        let mut mine = convo_msg(2, "me", "the second one, because it dedups cleanly");
        mine.from_is_bot = true;
        mine.from_is_human = false;
        mine.is_self = true;

        let t = render_transcript(&[alice, mine], None);
        assert!(!t.contains("⚠"), "{t}");
    }

    #[test]
    fn no_warning_in_a_chat_with_no_people_in_it() {
        let mut bot = convo_msg(1, "agent_b", "just us here");
        bot.from_is_bot = true;
        bot.from_is_human = false;
        assert!(!render_transcript(&[bot], None).contains("⚠"));
    }

    #[test]
    fn an_empty_chat_renders_a_note_rather_than_nothing() {
        assert!(render_transcript(&[], None).contains("no messages yet"));
    }

    #[test]
    fn the_header_lists_humans_and_agents_with_their_skills() {
        let alice = ParticipantView {
            participant: Participant {
                user_id: 1,
                name: Some("alice".into()),
                username: Some("alice".into()),
                is_bot: false,
                is_human: true,
                mentionable: true,
                message_count: 3,
                last_seen: 0,
            },
            is_self: false,
        };
        let bot = ParticipantView {
            participant: Participant {
                user_id: 2,
                name: Some("agent_b".into()),
                username: Some("agent_b".into()),
                is_bot: true,
                is_human: false,
                mentionable: true,
                message_count: 1,
                last_seen: 0,
            },
            is_self: true,
        };
        let agents = vec![KnownAgent {
            bot_id: 2,
            username: Some("agent_b".into()),
            name: Some("agent_b".into()),
            model: Some("claude-opus-5".into()),
            description: Some("Rust".into()),
            chat_id: 1,
            last_seen: 0,
        }];
        let header = TranscriptHeader {
            chat_type: "supergroup",
            title: Some("proj"),
            chat_username: Some("projchat"),
            pinned: Some("ship by friday"),
            participants: &[alice, bot],
            agents: &agents,
            listening: &HashSet::new(),
            older_cached: 4,
        };

        let t = render_transcript(&[convo_msg(9, "alice", "hi")], Some(&header));
        assert!(t.contains("supergroup · proj @projchat"), "{t}");
        assert!(t.contains("humans: @alice"), "{t}");
        // The agent line carries what it is good at, and marks the viewer.
        assert!(
            t.contains("agents: @agent_b (you) — claude-opus-5, Rust"),
            "{t}"
        );
        assert!(t.contains("pinned: ship by friday"), "{t}");
        assert!(t.contains("4 older"), "{t}");
    }

    #[test]
    fn someone_without_a_username_is_flagged_as_unmentionable() {
        let nameless = ParticipantView {
            participant: Participant {
                user_id: 3,
                name: Some("Toad".into()),
                username: None,
                is_bot: false,
                is_human: true,
                mentionable: false,
                message_count: 1,
                last_seen: 0,
            },
            is_self: false,
        };
        let header = TranscriptHeader {
            chat_type: "group",
            title: None,
            chat_username: None,
            pinned: None,
            participants: &[nameless],
            agents: &[],
            listening: &HashSet::new(),
            older_cached: 0,
        };
        let t = render_transcript(&[], Some(&header));
        assert!(t.contains("Toad (no username — reply to reach)"), "{t}");
    }
}
