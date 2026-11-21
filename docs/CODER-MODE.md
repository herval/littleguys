# Coder mode

Spec of November 19, 2026. Phases 1 to 4 and 6 are built on the Mac (see "Shipping it in pieces" for what's left); the phone is phase 5.

## In one line

With coder mode on, a thread whose folder is in a git repo shows where its work stands: the repo, the branch, what changed, whether it's committed, pushed and in a pull request, and whether checks pass. You move the work forward by asking the agent. littleguys reads git and never writes it.

Coder mode has its own tab in Settings, with one switch that turns all of it on or off. With it off, littleguys looks and behaves exactly as it does today and never runs `git`.

## Why, and why not another Conductor

Coding agents are where most littleguys threads come from: every Claude Code and Codex session on the Mac is already a thread, and each one has a folder. Today a coding thread looks like any other chat. Finding out what it actually did means opening a terminal or an editor in that folder, so the one glance littleguys promises stops working right where a coder needs it most.

The obvious way to fix that is to build a worktree workspace: make a branch per task, review the diff, open the PR, merge. That market is full. Conductor, Emdash, Orca, Claude Code desktop, the Codex app, Claude Squad, cmux and more all do it, and the site's own `/conductor-alternatives` page says littleguys "doesn't make worktrees, diffs or pull requests" and "works next to a workspace, not instead of one". Becoming one more workspace would cost the "most elegant, simplest, fastest" position and still come last on features.

What littleguys has that none of them do is that it follows every session wherever it started: a terminal, `claude --bg`, Codex's shared server, or one of those workspace apps (their sessions are Claude Code and Codex sessions too). It also follows them on the phone. So coder mode is a lens, not a workspace. It answers three questions about any coding thread, from anywhere, at a glance:

1. **Where is it?** Which repo and which branch.
2. **What did it do?** Which files, how many lines, and the diff.
3. **Is it shipped?** Committed, pushed, in a pull request, with checks passing.

Moving the work forward goes through the chat, like everything else in littleguys: one tap sends the agent "commit this" or "open a pull request", and the agent does it under its usual approvals.

## Principles

- **One switch.** Coder mode is off until you turn it on. Everything else on its settings page is optional.
- **Nothing to register.** Any thread whose folder is in a git repo gets coder mode, wherever the repo lives. There's no list of repos to keep up to date.
- **Read git, never write it.** littleguys runs only read-only git commands. Commits, pushes, branches and pull requests are things the agent does when you ask it to.
- **The engine's own features.** Worktrees come from `claude -w` and `codex --worktree`, and reviews from the guy's own review command when it has one. littleguys doesn't keep a parallel worktree registry or its own review prompts, the same rule as reusing OpenClaw's workspaces.
- **Works for sessions it didn't start.** A Claude Code session that Conductor started in its own worktree gets the same branch chip and Changes panel as one littleguys started.
- **Same on the phone.** Everything goes through runtime calls, so the bridge carries it to littleguys for iPhone without new protocol, and the Mac's settings apply there too.
- **Cheap.** It never polls in the background for threads you aren't looking at, and a status read costs one or two `git` processes per repo, cached across all the threads in that repo.

## Settings → Coder mode

Settings gains a sixth tab, **Coder mode**, between Phones and General. It's built like the Notifications and Phones tabs.

```
 Connections  Agents  Notifications  Phones  [Coder mode]  General

 ┌──────────────────────────────────────────────────────────┐
 │ Coder mode                                          [ ●] │
 │ Show each coding thread's branch, changes and pull       │
 │ request, and ask the agent to commit or open one.        │
 └──────────────────────────────────────────────────────────┘

 START CHATS IN
 ┌──────────────────────────────────────────────────────────┐
 │ 📁 shop      ~/dev/shop                               ✕  │
 │ 📁 blog      ~/dev/blog                               ✕  │
 │ ＋ Add a folder…                                          │
 └──────────────────────────────────────────────────────────┘
 These are always offered when you start a chat.

 ┌──────────────────────────────────────────────────────────┐
 │ Show the Changes panel                              [ ●] │
 │ Opens beside a coding thread when it has changes.        │
 │ New threads can use their own worktree              [○ ] │
 │ Adds "In a new worktree" when you start a thread.        │
 │ Show pull requests and checks                       [○ ] │
 │ Reads this branch's pull request with your gh login.     │
 │ Open a terminal from coding threads                 [ ●] │
 │ Adds a terminal button that opens the thread's folder.   │
 │   Open it in                                [ iTerm  ▾ ] │
 └──────────────────────────────────────────────────────────┘
```

### The switch

Coder mode is off by default. Turning it off hides every piece described below, stops every `git` and `gh` call at once, and leaves the other settings as they were, so turning it back on returns to the same setup.

So people find it, a Claude Code or Codex thread whose folder has a `.git` above it shows a one-time line above the composer while coder mode is off: "This chat works in a git repo. Turn on Coder mode to see its branch and changes." It has **Turn on** and **Not now**, and either answer makes it go away for good. Spotting the repo is a file check, not a `git` call.

### Start chats in

This list doesn't decide where coder mode works; any repo counts. It's the folders you start coding chats in.

littleguys can already start a chat in a chosen folder. A new Claude Code, Codex or local ACP thread shows **Works in ▾** above its composer until the first message, and a routine's editor has the same picker. It offers the guy's own folder, up to six folders its other threads used recently, and **Choose another folder…**. What it lacks is a way to keep a folder there. Recent folders come from the guy's current threads, so a folder drops off once its threads are archived or leave the list (a Claude Code terminal session leaves it three days after it ends), and a project you come back to weekly means the system dialog again.

- **In the picker:** these folders sit at the top of **Works in ▾**, above the recent ones, for every guy that can pick a folder. Each shows its repo's name, or the folder's name when it isn't a repo.
- **Adding one:** **Add a folder…** here opens the system folder picker. A recent folder in **Works in ▾** also gets a pin on hover, which adds it to this list without a trip to Settings.
- **Removing one:** the ✕ on its row, or the pin again in the picker. Threads that already work there aren't affected.
- **Empty:** that's the starting state, and the picker works as it does today.

With coder mode off, the picker goes back to today's list and the saved folders stay saved.

### Show the Changes panel

It's on by default. With it on, the Changes panel opens beside a coding thread on its own whenever the thread has changes, and closing it keeps it closed for that thread. With it off, the panel only opens when you click the `+42 −7` count in the chat header. The count, the branch and the rest still show either way.

### New threads can use their own worktree

It's off by default. With it on, the folder picker above a new thread's composer gains **In a new worktree** when the folder is a repo, for Claude Code and Codex guys (see "New threads in their own worktree" below). With it off, littleguys never passes a worktree flag, even if the phone asks.

### Show pull requests and checks

It's off by default, and only shows when `gh` is installed. With it on, littleguys asks `gh` for the pull request of each coding thread's branch, as described in "Pull requests and checks" below. That's the only part of coder mode that reaches the network, which is why it's a separate switch: the FAQ line saying littleguys fetches only its own updates gets "and, if you turn it on, your pull requests through gh".

### Open a terminal from coding threads

It's on by default, like the Changes panel: it adds one button and runs nothing until you click it. With it on, coding threads get the terminal button described in "A terminal in the thread's folder" below. With it off, the button, its shortcut and its menu item go away.

Under the switch, **Open it in ▾** picks the terminal app. It lists the terminals found on this computer, with the system one first and marked as the default (Terminal on a Mac; Windows Terminal on a PC, else PowerShell), and ends with **Choose another app…**, which opens the system picker in Applications. An app that doesn't say it opens folders gets a line under the picker: "Plainterm doesn't say it opens folders, so it may start in your home folder." The choice is kept even while the switch is off.

## What you see

Everything in this section shows only with coder mode on, for threads whose folder is in a git repo.

### 1. Threads grouped by project

A Claude Code or Codex guy whose threads work in more than one repo lists them under the repo's name. Worktrees fold under the repo they came from, with their branch on the row. With one repo, or none, the list stays as it is today.

```
 ◂ littleguys                      ＋  ⚙
 (🤖) Claude Code
      every session on this Mac
 [ Chats ][⏰][📁][✨]

 shop
   Fix the cart total rounding      2m
   ⑂ fix-rounding · +42 −7 · ●working
   Add a coupon field              1h
   ⑂ coupons · PR #31 ✓
 blog
   Draft the launch post           3h
   main · nothing changed
 Other chats
   What's on my calendar…          1d
```

### 2. The thread row

Under the title, where the preview sits, a coding thread shows its branch and one more fact, whichever matters most: working now, changes not yet committed (`+42 −7`), commits not yet pushed (`2 to push`), its pull request (`PR #31 ✓` or `PR #31 ✗ checks`), or `nothing changed`. Approvals and questions still take the row over, as they do today.

### 3. The chat header

The header gains a chip on the folder line: `shop · ⑂ fix-rounding`, and next to it a `+42 −7` button that opens the Changes panel. With a pull request, `PR #31 ✓` sits beside them and opens it in the browser.

### 4. The Changes panel

It's a panel on the right of the chat, the way the browser panel is today, and it is read-only.

```
 Changes · fix-rounding vs main            ✕
 ─────────────────────────────────────────
 Edited in this chat
   M  src/cart.rs                 +38 −7
   A  tests/rounding.rs           +4
 Also changed in this folder
   M  README.md                   +1 −1    ▸
 ─────────────────────────────────────────
 src/cart.rs
   41   let total = items.iter()…
   42 − total.round()
   42 + (total * 100.0).round() / 100.0
        💬 Comment
 ─────────────────────────────────────────
 [ Review ]  [ Commit ]  [ Open a PR ]
```

- **What it compares against:** the branch's base, so "changes" means everything this branch has that its main branch doesn't, committed or not. On a feature branch or worktree that's the whole task. On `main` it's whatever isn't pushed yet. Untracked files count.
- **Whose changes:** several threads often share one folder (everyone on `main`), so the panel splits files between those this thread edited, taken from its own tool calls in the transcript, and the rest of the folder's changes, folded.
- **Diffs:** the same renderer as the transcript's edit cards, moved out of `Transcript.tsx` so both use it.
- **Comment on a line:** a click on a line puts a quote in the composer, as `src/cart.rs:42` plus the line, and you type the rest. Reviewing is chatting, so there are no review threads to keep.

### 5. Asking for the next step

The bottom of the Changes panel, and a row of chips above the composer when a turn ends with uncommitted changes, offer the next step. Each one sends an ordinary message to the thread:

| Chip | Sends | Shown when |
|---|---|---|
| Review | The guy's own review command if it has one (Codex `/review`, a `code-review` or `review` skill on Claude Code), else "Review your changes so far and tell me what you'd fix." | There are changes |
| Commit | "Commit these changes with a clear message." | Uncommitted changes |
| Push | "Push this branch." | Commits not yet pushed, no PR |
| Open a PR | "Push this branch and open a pull request." | Branch isn't the base, no PR yet |
| Fix the checks | "Checks failed on PR #31: `<check names>`. Fix them." | PR checks failed (needs the pull request switch) |

Tapping a chip sends it straight away, because on the phone that's the point. Pushing and opening PRs still go through the agent's approvals, as they do now. Holding Option (or long-pressing on the phone) puts the text in the composer instead, so you can edit it first.

### 6. Pull requests and checks

With the pull request switch on, and the branch on GitHub, the header and row show the PR's number, whether it's a draft, its checks (passing, failing or running) and its review state. Clicking the chip opens the PR. If checks fail on a PR whose thread isn't open, the row gets the `Fix the checks` chip and an unread dot, but no notification: a failed check isn't the agent needing you. There's no CI log viewer; the agent reads the logs itself when you ask it to fix them.

### 7. The popover

The popover and the menu bar guy don't change in v1. A popover row is one guy's latest message, and a branch line there would crowd the one glance it's for.

### 8. New threads in their own worktree

With the worktree switch on, the folder picker above a new thread's composer gains one item when the folder is a repo: **In a new worktree**. The first message then starts the session with `claude -w <name> --bg` or `codex --worktree`, with a name made from the first words of the message. The thread is grouped under its repo like any other worktree. Removing the worktree stays with the engine: archiving a Claude Code worktree thread offers "Also delete its worktree", which runs `claude rm <id>` and refuses if anything isn't committed. Codex has no equivalent yet, so its worktrees stay where Codex put them.

### 9. A terminal in the thread's folder

Sooner or later you want a shell where the agent is working: to run the app, try the tests yourself, or poke at a branch. With the terminal switch on, that's one click.

```
 (🤖) Claude Code  Fix the cart total rounding
      ● online · in shop (worktree)      ⑂ fix-rounding  PR #31 ✓  +42 −7  ⌨  ⟳
```

- **Where:** a terminal button (`SquareTerminal`) in the chat header, after the `+42 −7` count, so it sits in the same place whether or not the folder is a repo. It's also **Open a terminal here** in a thread row's menu, and ⇧⌘T (Ctrl+Shift+T on a PC) in the open thread. Its tooltip names the folder: "Open a terminal in shop (⇧⌘T)".
- **Which threads:** every thread whose folder is on this computer: Claude Code, Codex and local ACP, the same ones that get a folder line in the header. The folder doesn't have to be a repo. Remote ACP, OpenClaw and Hermes threads don't get it, since their folder is on another machine or isn't theirs to open.
- **Which folder:** the thread's own, the one its agent works in, not the repo's root. A worktree thread opens the worktree, and a session started in `shop/web` opens `shop/web`.
- **What opens:** a new window or tab of the chosen terminal, as the app does for a folder, with your shell in it. littleguys doesn't type anything into it. It doesn't run `claude --resume` there either: the session is already running in littleguys or in the background, and two clients on one session fight.
- **When it can't:** a folder that's gone (an archived worktree, say) shows "/Users/me/dev/shop-wt doesn't exist on this Mac" under the button for a few seconds, the same message as Show in Finder; from the row menu it's a dialog. A terminal app that was uninstalled falls back to the system one, with a note saying so and to pick another in Settings.

### 10. The phone

The phone gets the branch line on rows, the Changes panel as a sheet, and the chips, all following the Mac's settings. Reviewing a diff and tapping "Open a PR" from the couch is the use case no worktree workspace covers. The settings page itself stays on the Mac in v1. The phone gets no terminal button: it would open a window on a Mac you're not sitting at.

## What it doesn't do

- No IDE. A file opened in the Files tab can be edited and saved (only over the version you opened, so it never lands on the agent's newer edit), but littleguys doesn't create, rename or delete files. No built-in terminal either: the terminal button opens the one you already use.
- No git writes: littleguys has no commit, push, merge, rebase or branch button. Every change goes through the agent.
- No merge button. Merging is a judgment call, and GitHub does it well.
- No worktree manager: it doesn't list, create or clean worktrees except through the engine's own flag and `claude rm`.
- No CI log viewer, no issue tracker, no task board, and no "run five agents on one task".
- It doesn't support GitLab or Bitbucket pull requests in v1 (the branch and changes still work there).
- Nothing for OpenClaw or Hermes guys in v1: their workspaces aren't repos as a rule, and remote ones have no git to read.

## How it works

### Settings, stored with the other prefs

The settings live in the app's prefs file next to `hiddenGuys` and `notifications` (`Prefs` in `src-tauri/src/activity/preferences.rs`), as one more field:

```json
"coder": {
  "on": false,
  "folders": ["/Users/me/dev/shop", "/Users/me/dev/blog"],
  "panel": true,
  "worktrees": false,
  "prs": false,
  "offered": false,
  "terminal": true,
  "terminalApp": "/Applications/iTerm.app"
}
```

`terminalApp` is null for the system terminal. `folders` is the "Start chats in" list, and `offered` records that the one-time "Turn on Coder mode" line was answered. `set_coder_prefs` in `commands.rs` (`core.setCoderPrefs` in `src/lib/ipc.ts`) saves them, and they travel on the roster as `roster.coder`, so the main window, the popover and paired phones get every change the way they get the notification settings. `code.rs` keeps the current value in a process-wide `RwLock`, and the adapters check it there. Two more commands serve the UI: `coder_tools` says whether `git` and `gh` are installed (the settings tab hides the pull request switch without `gh`), and `in_git_repo` answers the one-time offer with a file check.

### A `code` capability and three calls

`Capabilities` gains `code: bool`. It's on for Claude Code, Codex, and ACP agents on this machine, and off for OpenClaw, Hermes and remote ACP. It says what the runtime could do; the settings decide whether it does.

`Runtime::call` answers three new calls for every runtime with `code`, from the folder `Runtime::local_folder` resolves for the thread, and hands them to one shared module, the way `localfs.rs` serves the Files tab. The UI and paired phones both reach runtimes through `Runtime::call`, so neither needs anything else:

- `code.status {sessionKey}` → `{repo: null, why: "off"|"not-a-repo"|"no-git"}`, else:
  `{repo: {root, name, mainRoot?, worktree}, branch?, detached, head, base: {ref, sha}, upstream?, ahead, behind, commits, dirty, changes: {files, added, removed, untracked}, conflicted}`. `base.ref` is what the changes count from: the base branch, the upstream when on it, or `HEAD`. `commits` counts commits since then, and `dirty` the files not committed yet.
- `code.diff {sessionKey, path?}` → `{base, files: [{path, oldPath?, status: added|modified|deleted|renamed|untracked, added, removed, binary}], patch?}`. `patch` is the unified diff, for one file when `path` is given, else for everything under a size cap (256 KB, as for text files). The UI parses it with the existing `fromPatch` in `src/lib/diff.ts`.
- `code.pr {sessionKey}` → `{pr: null}` or `{pr: {number, url, title, state, draft, checks: passing|failing|running|none, failing?: [name], review?: approved|changes|pending}}`, plus `{unavailable: "off"|"no-gh"|"not-github"|"logged-out"}` so the UI can stay quiet.

Before running anything, `code.rs` checks that coder mode is on, and then that a `.git` sits in the thread's folder or above it, which is a file check. Otherwise it answers right away with the reason and starts no process, so a chat outside any repo costs nothing. Because these are runtime calls, the phone gets them through the bridge with no protocol change, and the Mac's settings hold there too.

### `src-tauri/src/code.rs`

It shells out to the `git` the user already has, not libgit2 or gix: that keeps their config, worktrees and `safe.directory` rules, and adds nothing to the binary. Every command runs with `--no-optional-locks` (or `GIT_OPTIONAL_LOCKS=0`), so a status read never takes `index.lock` while the agent is committing. If `git` isn't there (a Windows PC without Git for Windows), the answer is `{repo: null, why: "no-git"}`, and the settings page says Git wasn't found.

- **Repo:** `git rev-parse --show-toplevel --git-common-dir --abbrev-ref HEAD`. A common dir outside the top level marks a worktree, and its parent is `mainRoot`, which is how a worktree gets grouped under its repo in the thread list, even when Codex keeps it in its own folder.
- **Base:** `origin/HEAD`, else `origin/main`, else `origin/master`, else local `main` or `master`. The comparison point is `git merge-base HEAD <base>`. On the base branch itself it's the upstream (`@{u}`), so "changes" means unpushed. The resolved base is cached per repo.
- **Changes:** `git status --porcelain=v2 --branch -z` for branch, ahead/behind, untracked and conflicts, then `git diff --numstat -z <merge-base>` for line counts. On this repo (176 tracked files) the status, numstat and merge-base reads together took 50 to 140 ms, measured on November 19, 2026; caching the merge-base per HEAD leaves two processes per read.
- **Cache:** results are kept per repo root for 2 seconds, since many threads share one folder. Reads for the same root are coalesced into one.
- **Diff:** `git diff --no-color --no-ext-diff <merge-base> [-- path]`, plus untracked files read as created. Binary files are listed but not shown.
- **"Edited in this chat":** comes from the UI, which already knows the thread's edit tool calls and their paths (`fileDiffs` in `src/lib/diff.ts`), matched against the diff's file list.
### When it refreshes

- When a thread opens, and when the main window gets focus.
- When a turn ends (`chat` final, `sessions.changed` end) and 2 seconds after an edit or exec tool call ends, debounced.
- Thread rows in the list read status only when their turn ends or when the list is shown, never on a timer.
- `code.pr` runs on the same triggers, at most once a minute per branch, and only while the thread or its row is on screen, the way the popover reads usage limits.
- A settings change refreshes everything on screen once.

### Pull requests through `gh`

`gh pr view <branch> --json number,url,title,state,isDraft,reviewDecision,statusCheckRollup`, run in the repo, and only with the pull request switch on. littleguys never reads tokens and never calls the GitHub API itself. Without `gh`, logged out, or with a non-GitHub remote, `code.pr` answers `unavailable` and nothing shows.

### Worktree threads

`chat.send` on a new thread already takes `cwd`; it gains `worktree: true`, which the adapter honors only with the worktree switch on and a repo at that folder. The Claude Code adapter's `launch` then adds `-w <name>` next to `--bg`, and the session's cwd comes from `claude agents` and the hooks, as it already does. Codex's app server has no worktree option (its `thread/start` takes none), so a Codex worktree thread starts with `codex exec --worktree` instead of on the shared server. Its approvals then can't be answered in littleguys, the same as any `codex exec` session. The ACP adapter gets nothing, since ACP has no worktree concept.

### Opening a terminal

It's a Tauri command, not a runtime call, so the bridge never carries it and a paired phone can't open windows on the Mac. It lives in a new `src-tauri/src/terminal.rs` with two commands, both in `commands.rs`' handler list:

- `coder_terminals()` → `[{app, name, system, folders}]`: the terminals on this computer. On a Mac it looks in `/Applications`, `~/Applications` and `/System/Applications/Utilities` for known bundle ids (Terminal, iTerm2, Ghostty, Warp, WezTerm, kitty, Alacritty, Hyper, Tabby, Rex), plus the saved `terminalApp` if it's none of those. `folders` comes from the app's `Info.plist`: a `CFBundleDocumentTypes` entry for `public.folder`, `public.directory` or `public.item` means it opens folders handed to it. Terminal, iTerm and Rex say so; some newer terminals don't. On a PC it's Windows Terminal when `wt.exe` is on the path, PowerShell 7 (`pwsh`), Windows PowerShell, and Command Prompt.
- `open_terminal(gateway, sessionKey)`: finds the runtime with `hub.client`, checks it has `code`, that coder mode and the terminal switch are on, and resolves the folder with `Runtime::local_folder`, the same one `code.*` reads. It never takes a path or a command line from the webview. A folder that's gone gets the `reveal_path` message.

`terminal.rs` reads each app's `Info.plist` with the `plist` crate (already in the build through Tauri). Listing every app in `/Applications` that way takes about 60 ms; opening a terminal reads only the chosen app's.

How each terminal is started, as argv and never through a shell, so a folder named `it's "new"` is safe:

| Terminal | Started with |
|---|---|
| Opens folders (Terminal, iTerm2, Rex, Warp, and any app whose `Info.plist` says so) | `open -a <app> <folder>` |
| Ghostty, if its `Info.plist` doesn't list folders | `open -na <app> --args --working-directory=<folder>` |
| WezTerm | `open -na <app> --args start --cwd <folder>` |
| kitty | `open -na <app> --args --directory <folder>` |
| Alacritty | `open -na <app> --args --working-directory <folder>` |
| Anything else picked by hand | `open -a <app> <folder>`, with the Settings warning |
| Windows Terminal | `wt.exe -d <folder>` |
| PowerShell, Command Prompt | the program with `current_dir(<folder>)` and `CREATE_NEW_CONSOLE` |

The rule for each is a pure function from (terminal, folder) to a program and arguments, so the tests check the argv without starting anything. On a Mac it waits for `open`, which fails when the app can't start, and reports its error. On Windows it can't use `platform::std_command`, which adds `CREATE_NO_WINDOW`: a terminal's whole point is its window. Nor does it redirect a console's stdio, so the shell gets the new console's own. `terminalApp` must be a `.app` folder on a Mac or one of the four names on a PC; anything else is refused when it's saved.

### UI pieces

- `src/main/CoderMode.tsx`: the settings tab, mounted in `Settings.tsx` beside `Notifications` and `Phones`.
- `src/lib/code.ts`: types, the settings (loaded once, updated on `lg:coder-prefs`), and a Zustand store keyed by gateway and repo root, with `useCode(guy, sessionKey)` for the header and rows. It returns nothing while coder mode is off, so no other component checks the setting itself.
- `src/ui/Diff.tsx`: `DiffView`, `DiffRow` and `DiffStat`, moved from `Transcript.tsx`.
- `src/main/ChangesPanel.tsx`: the panel, mounted in `App.tsx` beside `ChatView` the way `BrowserPanel` is, when the guy has `code` and coder mode is on.
- `ChatView.tsx`: the branch chip and the stat button in the header, the chips above the composer, and the one-time "Turn on Coder mode" line.
- `Sidebar.tsx` `ThreadList`: grouping by repo for guys with `code`, and the branch line on rows.
- `FolderPicker.tsx`: the "Start chats in" folders at the top (in threads and in the routine editor, which share it), the pin on recent folders, and "In a new worktree".
- The terminal: a `TerminalButton` in `ChangesPanel.tsx`, after `RepoButtons`, shown where the header shows a folder; **Open a terminal here** in `Sidebar.tsx`' `ThreadMenu`; ⇧⌘T in `App.tsx`' key handler, sent to the open thread as `lg:open-terminal`; the switch and **Open it in ▾** in `CoderMode.tsx`; `useTerminalOn(gatewayId)` in `code.ts` so each of those checks one hook; `core.openTerminal` and `core.coderTerminals` in `ipc.ts`.

### Tests

- Rust: temporary repos made in tests, with fictional names (`shop`, `blog`), covering a clean repo, uncommitted and untracked changes, a branch ahead of its base, a worktree inside the repo and one outside it, a detached HEAD, a repo with no remote, no `git` on the path, and a conflicted merge. The settings checks get their own tests: coder mode off, and a folder with no `.git` above it, each answered without starting a process. `gh` gets a fake on the path that prints canned JSON.
- UI: the mock gateway (`src/lib/mock.ts`) gains `code.*` answers and coder prefs, so the settings tab, the Changes panel and the grouped list can be seen in the browser build.
- Terminal: the argv for each kind of terminal and for a folder with spaces and quotes; reading folder support from fixture `Info.plist` files (one with `public.directory`, one with `public.item`, one with none); `open_terminal` refusing with coder mode off, the switch off, a runtime without `code`, and a folder that's gone; an older prefs file without the two new fields loading with the switch on and the system terminal. The mock lists a few fictional terminals and logs instead of opening one. By hand before it ships: every terminal installed here, a worktree thread, a session started in a subfolder, and Windows Terminal and PowerShell from the `windows.yml` installer.

## Shipping it in pieces

Each phase ships alone and is useful by itself. The settings tab grows a switch with each phase, so it never shows one that does nothing.

1. **Where it stands.** The Coder mode tab with its switch, the "Start chats in" folders and the Changes panel switch, the one-time offer, the `code` capability, `code.status` and `code.diff`, the branch chip, the stat and the Changes panel, rows grouped by repo, and the branch line on rows. This phase is all read-only and needs no network. It's the biggest piece, about a week and a half.
2. **Ask for it.** The Review, Commit, Push and Open a PR chips, and comments on a line. About two days.
3. **Pull requests and checks.** The pull request switch, `code.pr` through `gh`, the PR chip, and Fix the checks. About two days.
4. **Worktree threads.** The worktree switch, "In a new worktree" in the folder picker, and `claude rm` on archive. Two or three days, more if Codex's server needs work.
5. **Phone.** The same pieces in littleguys for iPhone, in the iOS repo.
6. **A terminal here.** The terminal switch and picker, the header button, the row menu item and ⇧⌘T, `coder_terminals` and `open_terminal`. About a day and a half, half of it trying each terminal app. It doesn't depend on phase 5.

Built so far: phases 1 to 4 and 6, except `claude rm` on archive, which deletes files and waits for a try on real sessions. Not tried yet on real sessions: `claude -w` with `--bg`, and `codex exec --worktree`; both flags are in the CLIs' help. The terminal button hasn't opened a real terminal yet: Terminal, iTerm and Rex (the ones on this Mac) need a try in the app, and Windows Terminal and PowerShell one from the `windows.yml` installer. The mock (`make mock`, then `?claudecode=1&coder=1`) shows every piece with fictional repos.

## Decisions for herval

1. **Lens or workspace?** This spec is a lens: it reads git and asks the agent to act. The alternative is a Conductor-style workspace that makes worktrees per task and has its own merge button, which means more features but a crowded market and the opposite of the positioning. The recommendation is the lens.
2. **Off by default?** The spec has coder mode off until you turn it on, with a one-time offer on the first coding thread in a repo. The other way is on by default for anyone with a Claude Code or Codex guy, which more people would see but which would run `git` for people who never asked. The recommendation is off, with the offer.
3. **The site.** The `/claude-code`, `/conductor-alternatives` and `/compare` pages say littleguys doesn't do diffs or pull requests. After phase 1 they'd say it has a coder mode that shows each session's branch, changes and pull request, and still doesn't make worktrees itself or merge. The pitch stays "like WhatsApp, but for your agents".
