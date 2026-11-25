# Models and effort (design)

Status, 2026-11-24: built for Claude Code and Codex, as below.

- Codex: a new thread starts on what you pick, and a started one switches from its next turn (`threadModels`, `switchModels`). The list comes from the app server's `model/list`, else `~/.codex/models_cache.json`; the default from `config.toml`.
- Claude Code: a new thread starts on what you pick (`--model`, `--effort`). A started thread shows what it runs on but can't switch yet (`switchModels: false`; `sessions.patch` says so), until the probe in Open questions runs (`probe_claude.py` in that session's job folder).
- Either way, the composer's chip shows your pick, else what the last turn ran on (from the transcript or rollout), else the engine's own default. Defaults stay in each engine's own settings: no littleguys default per guy, and no routines, OpenClaw or ACP yet.

The rest is the proposal it was built from.

## Today

- OpenClaw: a guy's model can be picked in the guy editor (`agents.update {model}`, choices from `models.list`). Nothing per thread, and no effort.
- Claude Code, Codex, ACP, Hermes: `models: false`. Codex has a development-only `LITTLEGUYS_CODEX_MODEL`.
- No runtime exposes effort.

## What each engine takes

Checked on this Mac against claude 2.1.331, codex 0.171.0 (its app-server JSON schema) and OpenClaw 2026.9.4.

| | Choices | New thread | Started thread | What it runs on now |
|---|---|---|---|---|
| OpenClaw | `models.list`; thinking levels from `/think`'s choices in `commands.list` | `sessions.patch {key, model, thinkingLevel}` before the first message | the same patch | the session (to check: does `sessions.list` return both?) |
| Claude Code | no list: aliases `fable`, `opus`, `sonnet`, `haiku`, or a full name; effort `low`, `medium`, `high`, `xhigh`, `max` | `claude "<text>" --bg --model M --effort E` | `/model M` and `/effort E` (both `supportsNonInteractive`) sent as resume prompts, or `--model`/`--effort` on the resume if that keeps the same session (see Open questions) | each assistant line in the transcript: `message.model`, `effort` |
| Codex | `model/list`: each model's `supportedReasoningEfforts`, `defaultReasoningEffort`, `isDefault`, `hidden` | `thread/start {model}`, then `turn/start {effort}`; without the shared server, `codex exec -m M -c model_reasoning_effort=E` | `turn/start {model, effort}`, which sticks for later turns; `codex exec resume` takes the same flags. A session open in a terminal (reached with `codex queue`) can't be switched from here | the rollout's `turn_context`: `model`, `effort` |
| ACP | the agent's `configOptions` of category `model` and `thought_level` (older agents: `models`) | `session/set_config_option` right after `session/new` | the same | the agent's config-option updates |
| Hermes | later, through its own `/model` command | | | |

## One surface for every runtime

Added to the OpenClaw-shaped surface in `runtime/mod.rs`:

- `models.list {agentId, sessionKey?}` → `{models: [{id, name, efforts?, defaultEffort?, isDefault?}], efforts?, default: {model?, effort?}}`. A top-level `efforts` is for runtimes where effort doesn't depend on the model (Claude Code, OpenClaw).
- `chat.send {…, model?, effort?}`: a draft's first message carries them, the way it carries `cwd` and `worktree` now.
- `sessions.patch {key, model?: string|null, effort?: string|null}`: a started thread. `null` goes back to the guy's default. It applies from the next turn; a turn under way keeps the model it started on.
- Session rows get `model?` and `effort?`, read from the transcript (what the last turn really ran on, even for a session started in a terminal), and `pendingModel?`/`pendingEffort?` while a switch waits for the next turn.
- Capabilities: `models` keeps meaning "the guy's model can be picked". New: `threadModels` (a thread can switch) and `effort`.
- When a switch lands, the transcript gets a quiet line, "Switched to Opus · high", so you can see where it changed.

## In the app

- A chip inside the composer, bottom left: `Opus 5.5 · high`. It opens a popover with the models (Default first, saying what it resolves to), then a row of the efforts that model takes.
- New thread: the chip sits with the folder picker, and its choice goes with the first message.
- Started thread: a change shows as pending (dotted) until the next turn starts on it.
- When this thread can't switch (a Codex session open in a terminal, a Claude Code terminal session idle at its prompt), the chip is read-only and says why.
- Each guy's default: the guy editor for OpenClaw (gaining effort), and Settings › Agents for Claude Code and Codex ("New threads use …"), saved in `ClaudeCodeConfig` / `CodexConfig` as `model` and `effort`. This replaces `LITTLEGUYS_CODEX_MODEL`.
- Routines: a routine's payload gets `model?` and `effort?`. A nightly job on a small model at low effort is the main reason to want this.

`/model` and `/effort` in the composer do the same as the chip. OpenClaw has them natively (`/model`, `/think`). For Claude Code and Codex, littleguys lists them itself, with choices from `models.list`, and answers them with a `sessions.patch` and `status: "ok"`, no turn. That path already exists in `commands.ts` and `chat.send`.

## Order

1. Codex: a native list, per-turn overrides that stick, and both values in the rollout.
2. Claude Code: new threads and the default (the flags are verified). Switching a started thread, after the probe below.
3. OpenClaw: per-thread model and thinking level (the patch exists), and effort in the guy editor.
4. ACP config options.
5. Routines.

## Open questions

- Claude Code: does `claude "<text>" --resume <id> --bg --model M --effort E` continue the same session? `resume()` says flags on a resume start a copy. If they do, a switch costs nothing extra. If they don't, it's `/model` and `/effort` each as its own resume before your message, about 2 to 3 seconds each.
- Answering it means starting real Claude Code sessions. herval said yes, but auto mode blocks this session from starting them, so herval runs the probe.

## Slash commands today

- OpenClaw: yes, with argument choices (`/think low`).
- ACP: yes, whatever the agent advertises (`available_commands_update`).
- Hermes: yes with the local backend; no in API mode.
- Codex: no (`commands: false`). Its commands live in its terminal UI. The app server has calls for the useful ones: `thread/compact/start`, `review/start`, `skills/list`, `model/list`.
- Claude Code: partly. The menu lists `/compact`, your own and your plugins' commands, and skills. A command runs when it's a new thread's first message, or when the session is idle or ended (it's resumed with the command as its prompt). But:
  - Typed while a turn runs, it goes in through the Stop hook as text ("The person wrote to you from littleguys: /compact"), so a built-in doesn't run. A skill may, because the model sees its name.
  - Messages that waited are joined into one prompt, so `/compact` followed by "now fix the test" arrives as `/compact` with that text as its argument.
  - A terminal session idle at its prompt gets it as context with whatever is typed there next.
  - Built-ins that work headless (`/model`, `/effort`, `/fast`, `/context`, `/cost`) aren't listed.

Fixes, separate from the picker:
- Claude Code: a message that starts with `/` never goes into a Stop hook or a batch. It waits for the turn to end and goes alone. List the headless built-ins.
- Codex: list `/compact`, `/review`, `/model`, `/effort` and its skills, and map them to the app-server calls.
