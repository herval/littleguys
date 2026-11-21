// Coder mode's pieces of a thread (docs/CODER-MODE.md): the Changes panel beside the chat,
// the branch and change count in the header, the branch line on thread rows, the next-step
// chips, and the one-time offer to turn coder mode on. All of them read; changes are the
// agent's to make, when a chip or you ask.
import { CircleCheck, CircleDot, CircleX, Code2, GitBranch, GitPullRequest, RotateCw, SquareTerminal, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  baseName,
  type ChangedFile,
  compose,
  matchesEdited,
  nextSteps,
  type NextStep,
  openTerminal,
  type PullRequest,
  readDiff,
  refreshCode,
  type RepoDiff,
  type RepoStatus,
  saveCoderPrefs,
  useChangesOpen,
  useChangesPanel,
  useCoderPrefs,
  usePullRequest,
  useRepoStatus,
  useTerminalOn,
} from "../lib/code";
import { loadCommands } from "../lib/commands";
import { type FileDiff, fileDiffs, fromPatch } from "../lib/diff";
import { capsOf, gatewayOf, followsSessions } from "../lib/guys";
import { errorMessage, invoke } from "../lib/ipc";
import { shortcut } from "../lib/platform";
import { toItems } from "../lib/transcript";
import type { Guy } from "../lib/types";
import { useApp } from "../stores/app";
import { threadId, useChat } from "../stores/chat";
import { DiffStat, DiffView } from "../ui/Diff";
import { IconButton, Spinner } from "../ui/kit";
import { openExternal } from "../ui/openExternal";

const MIN_WIDTH = 340;

// ---- the panel ---------------------------------------------------------------------------

/** The Changes panel, when this thread's is open (nothing while coder mode is off or outside a repo). */
export function ChangesSide({ guy, sessionKey }: { guy: Guy; sessionKey: string }) {
  const status = useRepoStatus(guy.gatewayId, sessionKey);
  const open = useChangesOpen(guy.gatewayId, sessionKey, status);
  return open ? <ChangesPanel guy={guy} sessionKey={sessionKey} /> : null;
}

/** Beside the chat: what this thread's branch changed, file by file, with the next steps. */
export function ChangesPanel({ guy, sessionKey }: { guy: Guy; sessionKey: string }) {
  const gw = guy.gatewayId;
  const tid = threadId(gw, sessionKey);
  const status = useRepoStatus(gw, sessionKey);
  const pr = usePullRequest(gw, sessionKey);
  const width = useChangesPanel((s) => s.width);
  const setWidth = useChangesPanel((s) => s.setWidth);
  const setOpen = useChangesPanel((s) => s.setOpen);
  const [diff, setDiff] = useState<RepoDiff | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [othersOpen, setOthersOpen] = useState(false);
  const messages = useChat((s) => s.threads[tid]?.messages);

  // Read the diff again whenever the status says something moved.
  const version = status ? `${status.head}|${status.dirty}|${status.changes.files}|${status.changes.added}|${status.changes.removed}` : "";
  useEffect(() => {
    if (!version) return;
    let live = true;
    readDiff(gw, sessionKey)
      .then((d) => {
        if (!live) return;
        setDiff(d);
        setError(null);
      })
      .catch((e) => live && setError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [gw, sessionKey, version]);

  // The files this thread's own edit tool calls touched.
  const edited = useMemo(() => {
    const paths = new Set<string>();
    for (const it of toItems(messages ?? [], [])) {
      if (it.kind !== "tool") continue;
      for (const d of fileDiffs(it.call.name, it.call.arguments, it.result?.diffs, it.result?.text)) paths.add(d.path);
    }
    return paths;
  }, [messages]);

  const files = diff?.files ?? [];
  const mine = files.filter((f) => matchesEdited(f.path, edited));
  const others = files.filter((f) => !matchesEdited(f.path, edited));
  const shownPath = selected && files.some((f) => f.path === selected) ? selected : (mine[0] ?? others[0])?.path;
  const parsed = useMemo(() => (diff?.patch ? fromPatch(diff.patch) : []), [diff?.patch]);

  if (!status) return null;

  const startResize = (e: React.MouseEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startW = width;
    const move = (ev: MouseEvent) => setWidth(Math.min(Math.max(MIN_WIDTH, window.innerWidth - 520), Math.max(MIN_WIDTH, startW + (startX - ev.clientX))));
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  return (
    <aside
      style={{ width: Math.max(MIN_WIDTH, Math.min(width, window.innerWidth - 520)) }}
      className="relative flex shrink-0 flex-col border-l border-[var(--color-line)] bg-[var(--color-pane-solid)]"
    >
      <div onMouseDown={startResize} className="absolute -left-1 top-0 z-10 h-full w-2 cursor-col-resize transition-colors hover:bg-[var(--color-chip)]" aria-hidden />
      <div data-tauri-drag-region className="flex h-14 shrink-0 items-center gap-2 border-b border-[var(--color-line)] pl-4 pr-2">
        <div data-tauri-drag-region className="min-w-0 flex-1">
          <h3 data-tauri-drag-region className="font-display text-[13.5px] font-semibold leading-tight">
            Changes
          </h3>
          <p data-tauri-drag-region className="truncate text-[11.5px] leading-tight text-[var(--color-ink-3)]">
            {status.branch ?? "detached"} vs {baseName(status)}
            {status.commits > 0 && ` · ${status.commits} commit${status.commits === 1 ? "" : "s"}`}
            {status.dirty > 0 && ` · ${status.dirty} not committed`}
          </p>
        </div>
        <IconButton label="Read the changes again" onClick={() => refreshCode(gw, sessionKey, { force: true })}>
          <RotateCw size={13} />
        </IconButton>
        <IconButton label="Close" onClick={() => setOpen(tid, false)}>
          <X size={14} />
        </IconButton>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        {!diff && !error && (
          <div className="flex justify-center py-8">
            <Spinner size={16} />
          </div>
        )}
        {error && <p className="px-4 py-3 text-[12px] text-[var(--color-bad)]">{error}</p>}
        {diff && files.length === 0 && <p className="px-4 py-6 text-center text-[12.5px] text-[var(--color-ink-3)]">Nothing changed on this branch yet.</p>}
        {files.length > 0 && (
          <div className="border-b border-[var(--color-line)] py-1.5">
            {mine.length > 0 && <SectionTitle>Edited in this chat</SectionTitle>}
            {mine.map((f) => (
              <FileRow key={f.path} file={f} selected={f.path === shownPath} onClick={() => setSelected(f.path)} />
            ))}
            {others.length > 0 && mine.length > 0 && (
              <button onClick={() => setOthersOpen((o) => !o)} className="w-full text-left">
                <SectionTitle>
                  Also changed in this folder <span className="font-normal normal-case tracking-normal">{others.length}</span>
                  <span className="ml-auto font-normal normal-case tracking-normal">{othersOpen ? "Hide" : "Show"}</span>
                </SectionTitle>
              </button>
            )}
            {(mine.length === 0 || othersOpen) &&
              others.map((f) => <FileRow key={f.path} file={f} selected={f.path === shownPath} onClick={() => setSelected(f.path)} />)}
          </div>
        )}
        {shownPath && (
          <FilePatch
            key={shownPath}
            gatewayId={gw}
            sessionKey={sessionKey}
            file={files.find((f) => f.path === shownPath) as ChangedFile}
            parsed={parsed.find((d) => d.path === shownPath) ?? null}
            truncated={diff?.truncated ?? false}
            draftKey={tid}
          />
        )}
      </div>

      <NextSteps guy={guy} sessionKey={sessionKey} status={status} pr={pr} className="border-t border-[var(--color-line)] px-3 py-2.5" />
    </aside>
  );
}

function SectionTitle({ children }: { children: React.ReactNode }) {
  return <p className="flex items-center gap-1.5 px-4 pb-0.5 pt-1.5 text-[10.5px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">{children}</p>;
}

const STATUS_LETTER: Record<ChangedFile["status"], { letter: string; tone: string; title: string }> = {
  added: { letter: "A", tone: "text-[var(--color-ok)]", title: "Added" },
  untracked: { letter: "U", tone: "text-[var(--color-ok)]", title: "New, not added to git yet" },
  modified: { letter: "M", tone: "text-[var(--color-warn)]", title: "Modified" },
  deleted: { letter: "D", tone: "text-[var(--color-bad)]", title: "Deleted" },
  renamed: { letter: "R", tone: "text-[var(--color-accent-strong)]", title: "Renamed" },
  copied: { letter: "C", tone: "text-[var(--color-accent-strong)]", title: "Copied" },
};

function FileRow({ file, selected, onClick }: { file: ChangedFile; selected: boolean; onClick: () => void }) {
  const s = STATUS_LETTER[file.status];
  const slash = file.path.lastIndexOf("/");
  return (
    <button
      onClick={onClick}
      title={file.oldPath ? `${file.oldPath} → ${file.path}` : file.path}
      className={`flex w-full items-center gap-2 px-4 py-1 text-left text-[12px] transition ${selected ? "bg-[var(--color-chip)]" : "hover:bg-[var(--color-chip)]"}`}
    >
      <span title={s.title} className={`w-3 shrink-0 text-center font-mono text-[11px] font-semibold ${s.tone}`}>
        {s.letter}
      </span>
      <span className="min-w-0 flex-1 truncate">
        {slash >= 0 && <span className="text-[var(--color-ink-3)]">{file.path.slice(0, slash + 1)}</span>}
        <span className="font-medium">{file.path.slice(slash + 1)}</span>
      </span>
      {file.binary ? <span className="text-[11px] text-[var(--color-ink-3)]">binary</span> : <DiffStat added={file.added} removed={file.removed} />}
    </button>
  );
}

/** One file's diff; clicking a line quotes it in the composer. */
function FilePatch({
  gatewayId,
  sessionKey,
  file,
  parsed,
  truncated,
  draftKey,
}: {
  gatewayId: string;
  sessionKey: string;
  file: ChangedFile;
  parsed: FileDiff | null;
  truncated: boolean;
  draftKey: string;
}) {
  const [own, setOwn] = useState<FileDiff | null>(null);
  // Past the size cap the full patch isn't sent: this file's alone is.
  useEffect(() => {
    if (parsed || !truncated || file.binary) return;
    let live = true;
    readDiff(gatewayId, sessionKey, file.path)
      .then((d) => live && setOwn((d.patch ? fromPatch(d.patch) : []).find((x) => x.path === file.path) ?? null))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [gatewayId, sessionKey, file, parsed, truncated]);
  const diff = parsed ?? own;
  return (
    <div>
      <div className="sticky top-0 z-[1] truncate border-b border-[var(--color-line)] bg-[var(--color-pane-solid)] px-4 py-1.5 font-mono text-[11px] text-[var(--color-ink-2)]">
        {file.path}
      </div>
      {file.binary ? (
        <p className="px-4 py-3 text-[12px] text-[var(--color-ink-3)]">A binary file; there's nothing to show.</p>
      ) : file.status === "deleted" && !diff ? (
        <p className="px-4 py-3 text-[12px] text-[var(--color-ink-3)]">Deleted.</p>
      ) : diff ? (
        <DiffView
          diff={diff}
          header={false}
          lines={400}
          onLine={(line) => {
            const n = line.sign === "-" ? line.old : line.new;
            compose(draftKey, `${file.path}${n ? `:${n}` : ""}\n> ${line.text.trim()}\n`);
          }}
        />
      ) : (
        <div className="flex justify-center py-6">
          <Spinner size={14} />
        </div>
      )}
      {diff && <p className="px-4 pb-3 pt-1 text-[11px] text-[var(--color-ink-3)]">Click a line to comment on it.</p>}
    </div>
  );
}

// ---- next steps --------------------------------------------------------------------------

/** The guy's own review command, if it has one (`/review`, `/code-review`). */
function useReviewCommand(guy: Guy, sessionKey: string): string | null {
  const hasCommands = useApp((s) => capsOf(s.roster, guy.gatewayId).commands);
  const [cmd, setCmd] = useState<string | null>(null);
  useEffect(() => {
    if (!hasCommands) return;
    let live = true;
    loadCommands(guy.gatewayId, guy.id, sessionKey).then((list) => {
      const names = list.map((c) => c.name.toLowerCase());
      const name = ["review", "code-review"].find((n) => names.includes(n));
      if (live) setCmd(name ? `/${name}` : null);
    });
    return () => {
      live = false;
    };
  }, [hasCommands, guy.gatewayId, guy.id, sessionKey]);
  return cmd;
}

/**
 * Chips that ask the agent for the next step: review, commit, push, open a PR, fix the checks.
 * A click sends it; Option-click puts it in the composer to edit first.
 */
export function NextSteps({
  guy,
  sessionKey,
  status,
  pr,
  className = "",
}: {
  guy: Guy;
  sessionKey: string;
  status: RepoStatus;
  pr: PullRequest | null;
  className?: string;
}) {
  const tid = threadId(guy.gatewayId, sessionKey);
  const review = useReviewCommand(guy, sessionKey);
  const send = useChat((s) => s.send);
  const [sent, setSent] = useState<NextStep["id"] | null>(null);
  const steps = nextSteps(status, pr, review);
  if (!steps.length) return null;
  return (
    <div className={`flex flex-wrap items-center gap-1.5 ${className}`}>
      {steps.map((step) => (
        <button
          key={step.id}
          title={`${step.message}\n\nOption-click to edit it first.`}
          disabled={sent === step.id}
          onClick={(e) => {
            if (e.altKey) return compose(tid, step.message);
            setSent(step.id);
            send(tid, step.message, [])
              .catch(() => {})
              .finally(() => setTimeout(() => setSent(null), 1500));
          }}
          className={`rounded-full border px-2.5 py-1 text-[12px] font-medium transition disabled:opacity-50 ${
            step.id === "checks"
              ? "border-[var(--color-bad)]/40 text-[var(--color-bad)] hover:bg-[var(--color-bad)]/10"
              : "border-[var(--color-line)] text-[var(--color-ink-2)] hover:bg-[var(--color-chip)] hover:text-[var(--color-ink)]"
          }`}
        >
          {step.label}
        </button>
      ))}
    </div>
  );
}

// ---- the header and the rows -------------------------------------------------------------

function PrBadge({ pr, compact = false }: { pr: PullRequest; compact?: boolean }) {
  const icon =
    pr.state === "merged" ? (
      <GitPullRequest size={11} className="text-[var(--color-accent-strong)]" />
    ) : pr.checks === "passing" ? (
      <CircleCheck size={11} className="text-[var(--color-ok)]" />
    ) : pr.checks === "failing" ? (
      <CircleX size={11} className="text-[var(--color-bad)]" />
    ) : pr.checks === "running" ? (
      <CircleDot size={11} className="text-[var(--color-warn)]" />
    ) : (
      <GitPullRequest size={11} />
    );
  return (
    <span className="inline-flex items-center gap-1">
      PR #{pr.number}
      {icon}
      {!compact && pr.draft && <span className="text-[var(--color-ink-3)]">draft</span>}
      {!compact && pr.state === "merged" && <span className="text-[var(--color-ink-3)]">merged</span>}
      {compact && pr.checks === "failing" && "checks"}
    </span>
  );
}

/** The header's branch, a +/− count that opens the Changes panel, and the pull request. */
export function RepoButtons({ guy, sessionKey }: { guy: Guy; sessionKey: string }) {
  const gw = guy.gatewayId;
  const status = useRepoStatus(gw, sessionKey);
  const pr = usePullRequest(gw, sessionKey);
  const open = useChangesOpen(gw, sessionKey, status);
  const setOpen = useChangesPanel((s) => s.setOpen);
  if (!status) return null;
  const { added, removed, files } = status.changes;
  return (
    <div className="flex shrink-0 items-center gap-1">
      <span
        title={`${status.repo.name}${status.repo.worktree ? " (a worktree)" : ""}, on ${status.branch ?? "a detached HEAD"}`}
        className="flex max-w-[180px] items-center gap-1 rounded-full px-1.5 py-0.5 text-[11.5px] text-[var(--color-ink-3)]"
      >
        <GitBranch size={12} className="shrink-0" />
        <span className="truncate">{status.branch ?? status.head?.slice(0, 7) ?? "no commits"}</span>
      </span>
      {pr && (
        <button
          onClick={() => openExternal(pr.url)}
          title={`${pr.title} · open on GitHub`}
          className="rounded-full px-1.5 py-0.5 text-[11.5px] text-[var(--color-ink-2)] transition hover:bg-[var(--color-chip)]"
        >
          <PrBadge pr={pr} />
        </button>
      )}
      <button
        onClick={() => setOpen(threadId(gw, sessionKey), !open)}
        title={open ? "Hide the changes" : files ? `${files} file${files === 1 ? "" : "s"} changed · show the changes` : "Nothing changed yet"}
        className={`flex items-center gap-1 rounded-full px-2 py-0.5 text-[11.5px] transition hover:bg-[var(--color-chip)] ${open ? "bg-[var(--color-chip)]" : ""}`}
      >
        <Code2 size={12} className="shrink-0 text-[var(--color-ink-3)]" />
        {files ? <DiffStat added={added} removed={removed} /> : <span className="text-[var(--color-ink-3)]">no changes</span>}
      </button>
    </div>
  );
}

/** The header's terminal button: the chosen terminal, in this thread's folder (also ⇧⌘T, sent as
 * `lg:open-terminal`). Any folder on this computer, repo or not. */
export function TerminalButton({ guy, sessionKey, folder }: { guy: Guy; sessionKey: string; folder: string }) {
  const on = useTerminalOn(guy.gatewayId);
  const [note, setNote] = useState<{ text: string; bad: boolean } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  const run = useCallback(() => {
    const flash = (text: string | null, bad: boolean) => {
      if (!text) return;
      clearTimeout(timer.current);
      setNote({ text, bad });
      timer.current = setTimeout(() => setNote(null), 5000);
    };
    openTerminal(guy.gatewayId, sessionKey).then(
      (text) => flash(text, false),
      (e) => flash(errorMessage(e), true),
    );
  }, [guy.gatewayId, sessionKey]);
  useEffect(() => {
    if (!on) return;
    document.addEventListener("lg:open-terminal", run);
    return () => document.removeEventListener("lg:open-terminal", run);
  }, [on, run]);
  if (!on) return null;
  const name = folder.split(/[\\/]/).filter(Boolean).pop() ?? folder;
  return (
    <span className="relative flex">
      <IconButton label={`Open a terminal in ${name} (${shortcut("T", true)})`} onClick={run}>
        <SquareTerminal size={13} />
      </IconButton>
      {note && (
        <span
          role="status"
          onClick={() => setNote(null)}
          className={`animate-pop absolute right-0 top-full z-30 mt-1.5 w-max max-w-[300px] rounded-lg border border-[var(--color-line)] bg-[var(--color-pane-solid)] px-2.5 py-1.5 text-[11.5px] leading-snug shadow-lg ${
            note.bad ? "text-[var(--color-bad)]" : "text-[var(--color-ink-2)]"
          }`}
        >
          {note.text}
        </span>
      )}
    </span>
  );
}

/** `shop-api · fix-rounding` for the header's folder line; `fallback` when it isn't a repo. */
export function RepoPlace({ gatewayId, sessionKey, fallback }: { gatewayId: string; sessionKey: string; fallback: React.ReactNode }) {
  const status = useRepoStatus(gatewayId, sessionKey);
  if (!status) return <>{fallback}</>;
  return (
    <span className="truncate" title={status.repo.root}>
      · in {status.repo.name}
      {status.repo.worktree && " (worktree)"}
    </span>
  );
}

/** Under a thread row's title, in place of its preview: its branch and the one fact that matters most. */
export function RepoLine({ gatewayId, sessionKey, status, selected }: { gatewayId: string; sessionKey: string; status: RepoStatus; selected: boolean }) {
  const pr = usePullRequest(gatewayId, sessionKey);
  const fact = rowFact(status, pr);
  return (
    <p className={`flex min-w-0 items-center gap-1 truncate text-[12px] ${selected ? "text-white/80" : "text-[var(--color-ink-3)]"}`}>
      <GitBranch size={11} className="shrink-0 opacity-80" />
      <span className="truncate">{status.branch ?? status.head?.slice(0, 7) ?? "no commits"}</span>
      {fact && <span className="shrink-0">·</span>}
      {fact && <span className={`shrink-0 ${selected ? "[&_*]:!text-white" : ""}`}>{fact}</span>}
    </p>
  );
}

function rowFact(s: RepoStatus, pr: PullRequest | null): React.ReactNode {
  if (pr?.checks === "failing" && pr.state === "open") return <PrBadge pr={pr} compact />;
  if (s.dirty > 0) return <DiffStat added={s.changes.added} removed={s.changes.removed} />;
  if (s.ahead > 0) return `${s.ahead} to push`;
  if (pr) return <PrBadge pr={pr} compact />;
  if (s.changes.files > 0) return <DiffStat added={s.changes.added} removed={s.changes.removed} />;
  return "nothing changed";
}

// ---- turning it on -----------------------------------------------------------------------

/**
 * Over the composer of a Claude Code or Codex thread working in a repo, while coder mode is
 * off: a one-time offer to turn it on. Spotting the repo is a file check in the core.
 */
export function CoderOffer({ guy, folder }: { guy: Guy; folder: string | null }) {
  const prefs = useCoderPrefs();
  const kind = useApp((s) => gatewayOf(s.roster, guy.gatewayId)?.status.kind);
  const [repo, setRepo] = useState(false);
  const eligible = !prefs.on && !prefs.offered && followsSessions(kind) && Boolean(folder);
  useEffect(() => {
    if (!eligible || !folder) return;
    let live = true;
    invoke<boolean>("in_git_repo", { folder })
      .then((yes) => live && setRepo(yes))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [eligible, folder]);
  if (!eligible || !repo) return null;
  return (
    <div className="mx-5 mb-1.5 flex items-center gap-2 rounded-lg bg-[var(--color-chip)] px-3 py-2 text-[12px]">
      <GitBranch size={13} className="shrink-0 text-[var(--color-ink-3)]" />
      <span className="min-w-0 flex-1 text-[var(--color-ink-2)]">This chat works in a git repo. Turn on Coder mode to see its branch and changes.</span>
      <button
        onClick={() => saveCoderPrefs({ on: true, offered: true })}
        className="shrink-0 rounded-full bg-[var(--color-accent)] px-2.5 py-0.5 font-medium text-white transition hover:brightness-110"
      >
        Turn on
      </button>
      <button onClick={() => saveCoderPrefs({ offered: true })} className="shrink-0 rounded-full px-2 py-0.5 text-[var(--color-ink-3)] transition hover:text-[var(--color-ink)]">
        Not now
      </button>
    </div>
  );
}
