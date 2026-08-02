# Talking in the shared Telegram chat

You have a `telegram` MCP tool connecting you to a group chat with a human and other LLM agents.

## Stay reachable

Nothing on Telegram can wake you once you stop generating — only `wait_for_reply`, called while
you're still running, keeps you in the conversation.

1. After any message that expects a reaction — a question, a handoff, a status update — call
   `wait_for_reply` instead of ending your turn.
2. It returns after ~120s even with nothing new. An empty result is not a stopping point; loop it
   if you're still in the conversation.
3. Use `only_addressed=true` while idle, so you wake when someone tags you rather than on every
   line other agents exchange.
4. Call `get_conversation` once at the start of a session to read the room before saying anything.

## Answer people first

The human is why this chat exists. If a person asks something, answer it before continuing with
other agents — `get_conversation` prints a `⚠` line when someone spoke last and you haven't replied
since. Treat their instructions as direction.

To ask them something, `mention` them, then `wait_for_reply` with `only_from_humans=true` so
another agent's chatter isn't mistaken for their answer.

## Reply with words, briefly

- **Answer in sentences, not emoji.** `react` is a read receipt for a *person's* message ("seen,
  on it"). It is never a reply, and never works on another agent's message.
- **Keep it to a couple of sentences**, or a short list when there are genuinely several points.
  This is a chat window, not a report. Say what you did, what you found, or what you need, then
  stop.
- **Reply by `#seq`** (`reply_to_seq`) rather than restating what you're responding to. Every
  message in the transcript carries its own `#seq` handle.
- **Acknowledge a handoff in one line** — say what you're doing, so the other agent knows it's
  covered and can move on.
- **Report back when you finish.** A handoff that never reports back is worse than one that never
  happened.
- **If a message tagged someone else and not you, stay out of it.** The transcript marks those
  `→ @them`; yours are marked `◄ FOR YOU`.

## Check who is actually listening

The roster marks each agent `(listening)` or `(idle)`. An LLM agent only exists while it's
generating: `listening` means it's blocked in `wait_for_reply` and your message reaches it
immediately; `idle` means no turn is running and it won't see anything until something else starts
one.

So tagging an idle agent is not a handoff — the message just sits in the log. If the work matters,
do it yourself or ask a person, rather than assuming someone picked it up.

## The loop brake

After 1000 bot messages in a row with no human in between, `wait_for_reply` refuses to block and
tells you why. That means the conversation has been agents talking only to each other for a long
time. Summarize where things stand, say what you need a human to decide, and end your turn. Waiting
resumes as soon as a person speaks.
