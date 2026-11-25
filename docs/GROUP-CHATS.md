# Group chats

Plan of November 21, 2026. Phases 1 and 2 are built on the Mac (`src-tauri/src/runtime/rooms`, `src/main/Room.tsx`), tested against scripted guys and the browser mock. No room has run with a real bot yet: see "To check" and "Known gaps".

## In one line

A room is a chat with you and several guys in it. littleguys keeps the room's transcript and decides who gets a turn; each guy takes part from an ordinary thread of its own, on whatever engine it runs.

## What the code allows

- **No engine has a shared session.** OpenClaw, Hermes, ACP, Claude Code and Codex each give a guy its own sessions, and the guys in one room can sit on different machines. So the room can't live in an engine. littleguys has to run it, the way `runtime/scheduler.rs` runs routines for engines without cron.
- **OpenClaw's own groups don't apply directly.** They hang off messaging channels (WhatsApp, Discord, Slack), not `chat.send`. But their conventions are the ones to reuse: one isolated session per agent per group, the "messages since your last reply" context block, sender labels on every line, `mention` vs `always` activation, and the exact silent token `NO_REPLY` (which `chat.history` already drops).
- **Every adapter already speaks one surface** (`runtime/mod.rs`): `sessions.*`, `chat.send|history|abort`, and the `chat` / `session.message` events. `outbox.rs` already holds messages for a busy thread, and `ChatMessage.senderLabel` already names who sent a line.

## The shape

**A room is a sixth runtime.** A built-in "Rooms" gateway (`runtime/rooms.rs`, `Runtime::Rooms`) whose sessions are rooms. It answers `sessions.list`, `chat.history`, `chat.send`, `chat.abort` and emits `chat` and `session.message` like any other. The transcript view, composer, sidebar, notifications, search and the phone bridge then work on rooms with small additions, not a second chat stack.

**Each member has a seat.** A seat is a normal thread of that guy, made with `sessions.create` on its own gateway. The guy works there with its own tools, workspace, memory and approvals. Seats stay out of the guy's thread list but open from the room ("Open Patch's side"), where its steps and diffs show as they do today.

**The room owns the transcript.** `rooms.json` (members, each one's attention, seat keys, read cursors) and one `rooms/<id>.jsonl` per room, in littleguys' data folder. Nothing is written into an engine's prompt files or config.

**What a guy receives** when it gets a turn is one message in its seat, in OpenClaw's group format:

```
[Room "Launch" · Herval (you answer to them), Scout (research), Patch (code) · you are Patch]
[Since your last turn]
Herval: the signup page 500s on Safari
Scout: it's the date parser, see sentry link
[Now, to you] Herval: @Patch can you fix it?
```

Only what's new since its cursor is sent. The standing rules go in once, on the seat's first turn and again when members change: how to address someone (`@Name`), that its final text is what the room sees, and that the exact reply `NO_REPLY` means "nothing to add". If a seat is reset or compacted, the last 50 room lines are replayed.

**What the room takes back** is the turn's final assistant text. `NO_REPLY` (or an empty turn) posts nothing. `@Name` in the text is who it's addressed to.

## Who speaks, and when

A cap on replies only limits the damage. What makes a group chat work is that speaking needs a reason. The room gives a turn for exactly three reasons, and a conversation ends when no reason is left.

### 1. You were addressed

A message is addressed to a guy when it `@mentions` it (`@everyone` is all of them). A message of yours that names nobody goes to whoever spoke last, since that's who you're answering; the composer shows who that is ("to Patch") and lets you pick someone else. Mentions work whoever wrote the message, so guys pull each other in the same way you do.

An addressed guy always gets a turn. It can still answer `NO_REPLY` (a "thanks" needs no answer), and its rules tell it not to acknowledge or sign off.

### 2. You hold the room

Each room has a lead: the guy that takes a message addressed to nobody while nobody has spoken yet. It defaults to the first member and you can change it. This is the cheap default, and it mirrors how a team works: one owner answers, and brings in others by name when it needs them.

### 3. You're listening, and the room went quiet

Each member has an attention setting, per room:

- **On call** (default): gets a turn only when addressed. Costs nothing otherwise. When it is called it sees everything it missed.
- **Listening**: also gets one look when the room goes quiet.

"Quiet" is the moment every addressed turn has finished and nobody is owed an answer. Then each listening guy that hasn't spoken since your last message gets one look, one after another, at the whole exchange, with: "Speak only if you'd change the outcome: a mistake, something nobody has said, a blocker. Otherwise `NO_REPLY`." Because looks are sequential, the second listener sees what the first one added and doesn't repeat it.

If a listener speaks and addresses someone, that guy is owed a turn (reason 1) and the room is busy again. When it goes quiet again, only listeners that haven't had their look yet get one.

### Why this ends

- **Unprompted speech is bounded by structure.** A guy gets at most one unaddressed look per message of yours. With n listeners, that is at most n unprompted messages, then silence until you speak.
- **Prompted speech is bounded by need.** A guy-to-guy chain continues only while each reply addresses someone and the receiver judges it needs an answer.
- **The backstop.** If guys have addressed each other 8 times since your last message, the room pauses with "They're still going. Let them continue?" in the transcript. This is the safety net, not the mechanism: in a healthy room it never shows.

### The rest of the turn-taking

- **You interrupt everything.** Your message starts a new round. Looks that hadn't happened yet are dropped (the new round will offer them again). A guy that's mid-turn and addressed gets it steered in where the engine can (`steer`), else it waits in the outbox as it does today.
- **Work doesn't block the room.** The floor is about deciding to speak. A guy on a 20-minute coding turn holds only its own seat; others keep talking, and a look doesn't wait more than 30 seconds for a running turn.
- **Several guys addressed at once** (`@Scout @Patch`) run in parallel, like OpenClaw's broadcast default. Each sees the other's answer on its next turn.
- **Writing again while a guy works** reaches it in the same turn where its bot can take that, and otherwise gives it one more turn with everything you wrote meanwhile.
- **A folder per member.** A guy whose threads each work in a folder (Claude Code, Codex, ACP) can be told which one its seat uses, from its menu in the room.
- **A guy that's offline** is skipped with a muted note; it catches up from its cursor when called again.
- **Rooms run while littleguys runs,** like local routines. A turn under way when it quits finishes in its seat, but the room doesn't collect it (see "Known gaps").

## In the app

- **Sidebar:** rooms sit among the guy cards, with stacked avatars. "New room" picks members, a name and the lead.
- **Transcript:** each guy's message carries its avatar and name; who it's addressed to shows as a chip. A quiet line says who is typing or working ("Patch is working · Scout is reading").
- **Composer:** `@` opens a member picker. A small label shows who an unaddressed message will go to ("to Patch"), and tapping it changes that.
- **Header:** member avatars; tap one to set On call or Listening, make it lead, open its side, or remove it.
- **Approvals** raised in a seat show in the room, attributed to the guy.
- **Phone:** the bridge already passes every runtime through, so rooms arrive as one more bot. The iPhone app needs the author rendering and the `@` picker.

## Shipping it in pieces

1. **Rooms, addressed turns and the lead.** `runtime/rooms.rs`, seats, the envelope, `NO_REPLY`, the transcript with authors, the `@` picker, new-room flow. Already useful: you direct, and guys can call each other.
2. **Listening.** The quiet look, your interrupt, the backstop prompt, per-member attention.
3. **Around it.** Phone rendering, search across rooms, notification prefs per room, seats' approvals in the room.
4. **Maybe: room tools.** A local MCP server (`room.post`, `room.read`) handed to engines that take one per session (ACP, Claude Code, Codex), so a guy can post progress mid-turn. Not needed for anything above, and not possible for remote gateways.

## To check before building

- **`NO_REPLY` on each engine.** OpenClaw honors it natively. Claude Code, Codex, Hermes and ACP agents need a test that they return it exactly, and the room should accept near misses (quotes, trailing period).
- **What a look costs** on the heavy engines: one resumed turn in a seat with only the new lines. Measure on Claude Code and Codex before making Listening easy to switch on for everyone.
- **Seat threads on OpenClaw:** that `sessions.create` threads can stay hidden from the list without archiving them.

## Known gaps

- **Quitting mid-turn:** the reply stays in the guy's seat and never reaches the room, and a paused room forgets the turns it held.
- **A folder Claude Code doesn't trust:** the room says so and points at the guy's seat, where you trust it; the reply that follows stays there.
- **No streaming:** a guy's message shows when its turn ends. Meanwhile the room says who's working.
- **Leaving:** a removed member's seat, and every seat of a deleted room, becomes an ordinary thread of that guy.
- **Not built:** attachments, the phone's rendering, search inside rooms.
- **Probing with real bots:** `examples/rooms_probe.rs` starts real Claude Code and Codex sessions. Run it only on purpose, and briefly.

## Decisions

- **Default attention** for a new member: On call (built), zero idle cost.
- **Seats:** hidden and opened from the room (built).
- **Guys addressing you:** whether a guy's `@Herval` notifies even when replies are muted for it (recommended: yes, it's a question to you).
