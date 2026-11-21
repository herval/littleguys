// Coder mode in the UI: its settings, and each coding thread's repo, branch, changes and pull
// request, read with the `code.*` calls (src-tauri/src/code.rs). See docs/CODER-MODE.md.
//
// Everything here is quiet while coder mode is off or the runtime can't read folders: the
// hooks answer null and nothing is asked, so no other component checks the setting itself.
import { useEffect } from "react";
import { create } from "zustand";
import { useApp } from "../stores/app";
import { threadId } from "../stores/chat";
import { capsOf } from "./guys";
import { core, gwCall, listen } from "./ipc";
import { COMPUTER } from "./platform";
import type { CoderPrefs } from "./types";

export const CODER_OFF: CoderPrefs = {
  on: false,
  folders: [],
  panel: true,
  worktrees: false,
  prs: false,
  offered: false,
  terminal: true,
  terminalApp: null,
};

export function useCoderPrefs(): CoderPrefs {
  return useApp((s) => s.roster.coder ?? CODER_OFF);
}

export function saveCoderPrefs(patch: Partial<CoderPrefs>): Promise<void> {
  return core.setCoderPrefs({ ...(useApp.getState().roster.coder ?? CODER_OFF), ...patch });
}

/** Whether coder mode reads this gateway's threads. */
export function useCoderOn(gatewayId: string | null | undefined): boolean {
  return useApp((s) => Boolean(gatewayId) && (s.roster.coder?.on ?? false) && capsOf(s.roster, gatewayId).code);
}

/** Whether this gateway's threads get the terminal button: coder mode and its terminal switch on, and folders on this computer. */
export function useTerminalOn(gatewayId: string | null | undefined): boolean {
  return useApp(
    (s) => Boolean(gatewayId) && (s.roster.coder?.on ?? false) && (s.roster.coder?.terminal ?? true) && capsOf(s.roster, gatewayId).code,
  );
}

/** Open the chosen terminal in a thread's folder. Resolves to a note worth showing when the chosen one is gone. */
export async function openTerminal(gatewayId: string, sessionKey: string): Promise<string | null> {
  const { opened, missing } = await core.openTerminal(gatewayId, sessionKey);
  return missing ? `${missing} isn't on this ${COMPUTER} anymore, so ${opened} opened. Pick another in Settings → Coder mode.` : null;
}

export interface RepoStatus {
  repo: { root: string; name: string; mainRoot: string | null; worktree: boolean };
  /** null when detached. */
  branch: string | null;
  detached: boolean;
  /** null in a repo with no commits yet. */
  head: string | null;
  /** What the changes are counted from: the base branch, the upstream (on the base branch), or "HEAD". */
  base: { ref: string; sha: string };
  upstream: string | null;
  ahead: number;
  behind: number;
  /** Commits since the base. */
  commits: number;
  /** Files not committed yet. */
  dirty: number;
  /** Everything since the base, committed or not. */
  changes: { files: number; added: number; removed: number; untracked: number };
  conflicted: number;
}

type StatusAnswer = RepoStatus | { repo: null; why: "off" | "not-a-repo" | "no-git" };

export type FileStatus = "added" | "modified" | "deleted" | "renamed" | "copied" | "untracked";

export interface ChangedFile {
  path: string;
  oldPath?: string | null;
  status: FileStatus;
  added: number;
  removed: number;
  binary: boolean;
}

export interface RepoDiff {
  base: RepoStatus["base"] | null;
  files: ChangedFile[];
  /** The unified diff; null when it was too big to send (`truncated`). */
  patch: string | null;
  truncated: boolean;
}

export interface PullRequest {
  number: number;
  url: string;
  title: string;
  state: "open" | "closed" | "merged";
  draft: boolean;
  checks: "passing" | "failing" | "running" | "none";
  failing: string[];
  review: "approved" | "changes" | "pending" | null;
}

type PrAnswer = { pr: PullRequest | null } | { unavailable: "off" | "no-gh" | "not-github" | "logged-out" };

interface Entry<T> {
  value: T;
  at: number;
}

interface CodeStore {
  /** By thread (`gateway|sessionKey`): its repo's status, or null outside a repo. */
  status: Record<string, Entry<RepoStatus | null>>;
  prs: Record<string, Entry<PullRequest | null>>;
}

const useCode = create<CodeStore>(() => ({ status: {}, prs: {} }));

const inFlight = new Map<string, Promise<void>>();

/** A status read again sooner than this is the one we have (git's own cache is 2 s too). */
const FRESH_MS = 1500;
/** `gh` is asked at most this often per thread (the core keeps it a minute per repo). */
const PR_FRESH_MS = 60_000;

/** Read a thread's repo status (and its pull request, with that switch on) again. */
export function refreshCode(gatewayId: string, sessionKey: string, opts: { force?: boolean } = {}): Promise<void> {
  const { roster } = useApp.getState();
  const prefs = roster.coder ?? CODER_OFF;
  if (!prefs.on || !capsOf(roster, gatewayId).code) return Promise.resolve();
  const key = threadId(gatewayId, sessionKey);
  const had = useCode.getState().status[key];
  if (!opts.force && had && Date.now() - had.at < FRESH_MS) return Promise.resolve();
  const running = inFlight.get(key);
  if (running) return running;
  const job = (async () => {
    try {
      const answer = await gwCall<StatusAnswer>(gatewayId, "code.status", { sessionKey });
      const status = answer.repo ? (answer as RepoStatus) : null;
      useCode.setState((s) => ({ status: { ...s.status, [key]: { value: status, at: Date.now() } } }));
      const pr = useCode.getState().prs[key];
      if (status && prefs.prs && status.branch && (!pr || Date.now() - pr.at > PR_FRESH_MS)) {
        const answer = await gwCall<PrAnswer>(gatewayId, "code.pr", { sessionKey });
        const value = "pr" in answer ? answer.pr : null;
        useCode.setState((s) => ({ prs: { ...s.prs, [key]: { value, at: Date.now() } } }));
      }
    } catch {
      // A thread that's gone, or git failing for a moment: keep what we had.
    } finally {
      inFlight.delete(key);
    }
  })();
  inFlight.set(key, job);
  return job;
}

/** Threads on screen, for refreshing them when the window comes back. */
const shown = new Map<string, number>();

/**
 * A thread's repo status, read when it shows and again whenever `refreshWhen` changes
 * (a row passes its running state, so a turn's end reads it again). Null outside a repo,
 * with coder mode off, or before the first read.
 */
export function useRepoStatus(gatewayId: string | null | undefined, sessionKey: string | null | undefined, refreshWhen?: unknown): RepoStatus | null {
  const on = useCoderOn(gatewayId);
  const prs = useApp((s) => s.roster.coder?.prs ?? false);
  const key = gatewayId && sessionKey ? threadId(gatewayId, sessionKey) : "";
  useEffect(() => {
    if (!on || !gatewayId || !sessionKey) return;
    refreshCode(gatewayId, sessionKey);
    shown.set(key, (shown.get(key) ?? 0) + 1);
    return () => {
      const n = (shown.get(key) ?? 1) - 1;
      if (n > 0) shown.set(key, n);
      else shown.delete(key);
    };
  }, [on, prs, gatewayId, sessionKey, key, refreshWhen]);
  return useCode((s) => (on && key ? (s.status[key]?.value ?? null) : null));
}

export function usePullRequest(gatewayId: string | null | undefined, sessionKey: string | null | undefined): PullRequest | null {
  const on = useApp((s) => (s.roster.coder?.on ?? false) && (s.roster.coder?.prs ?? false));
  const key = gatewayId && sessionKey ? threadId(gatewayId, sessionKey) : "";
  return useCode((s) => (on && key ? (s.prs[key]?.value ?? null) : null));
}

/** Every known status, for grouping a guy's threads by repo. */
export function useRepoStatuses(): Record<string, Entry<RepoStatus | null>> {
  return useCode((s) => s.status);
}

export function readDiff(gatewayId: string, sessionKey: string, path?: string): Promise<RepoDiff> {
  return gwCall<RepoDiff>(gatewayId, "code.diff", path ? { sessionKey, path } : { sessionKey });
}

/** Where a status's repo belongs in a list: a worktree under the repo it came from. */
export function repoKey(status: RepoStatus): string {
  return status.repo.mainRoot ?? status.repo.root;
}

/** The base branch's short name, for "vs main". */
export function baseName(status: RepoStatus): string {
  return status.base.ref === "HEAD" ? "last commit" : status.base.ref.replace(/^origin\//, "");
}

/** Whether a status's branch is the one its changes are counted from (working on main). */
export function onBaseBranch(status: RepoStatus): boolean {
  return !status.branch || status.base.ref === "HEAD" || status.base.ref === status.upstream || baseName(status) === status.branch;
}

// A turn's end and a file edit change the folder: read the thread's status again.
let wired = false;
const editTimers = new Map<string, number>();
export function connectCodeStore() {
  if (wired) return;
  wired = true;
  listen<{ gateway: string; event: string; payload: Record<string, unknown> }>("gw:event", ({ gateway, event, payload }) => {
    const sessionKey = typeof payload?.sessionKey === "string" ? payload.sessionKey : null;
    if (!sessionKey) return;
    const key = threadId(gateway, sessionKey);
    if (!useCode.getState().status[key]) return;
    if (event === "chat" && ["final", "error", "aborted"].includes(String(payload.state))) {
      window.setTimeout(() => refreshCode(gateway, sessionKey, { force: true }), 400);
    } else if (event === "agent") {
      const data = (payload.data ?? {}) as Record<string, unknown>;
      if ((data.kind !== "tool" && payload.stream !== "tool") || data.phase === "start" || data.phase === "update") return;
      window.clearTimeout(editTimers.get(key));
      editTimers.set(key, window.setTimeout(() => refreshCode(gateway, sessionKey, { force: true }), 2000));
    }
  }).catch(() => {});
  window.addEventListener("focus", () => {
    for (const key of shown.keys()) {
      const i = key.indexOf("|");
      refreshCode(key.slice(0, i), key.slice(i + 1));
    }
  });
}

// ---- the Changes panel, and asking for the next step -------------------------------------

const WIDTH_KEY = "littleguys.changes.width";

interface PanelStore {
  /** Opened or closed by hand, per thread; otherwise it follows the "Show the Changes panel" switch. */
  chosen: Record<string, boolean>;
  width: number;
  setOpen(tid: string, open: boolean): void;
  setWidth(width: number): void;
}

export const useChangesPanel = create<PanelStore>((set) => ({
  chosen: {},
  width: (() => {
    try {
      return Number(localStorage.getItem(WIDTH_KEY)) || 460;
    } catch {
      return 460;
    }
  })(),
  setOpen: (tid, open) => set((s) => ({ chosen: { ...s.chosen, [tid]: open } })),
  setWidth: (width) => {
    try {
      localStorage.setItem(WIDTH_KEY, String(width));
    } catch {
      // Not kept, then.
    }
    set({ width });
  },
}));

/** Whether a thread's Changes panel shows: as you left it, else on its own once it has changes. */
export function useChangesOpen(gatewayId: string, sessionKey: string, status: RepoStatus | null): boolean {
  const tid = threadId(gatewayId, sessionKey);
  const chosen = useChangesPanel((s) => s.chosen[tid]);
  const auto = useApp((s) => s.roster.coder?.panel ?? true);
  if (!status) return false;
  return chosen ?? (auto && status.changes.files > 0);
}

/** Put text in a thread's composer (after what's there), for you to finish and send. */
export function compose(draftKey: string, text: string) {
  document.dispatchEvent(new CustomEvent("lg:compose", { detail: { draftKey, text } }));
}

export interface NextStep {
  id: "review" | "commit" | "push" | "pr" | "checks";
  label: string;
  message: string;
}

/**
 * What to ask the agent next, given where the branch stands. `review`: the guy's own
 * review command (`/review`), else it's asked in words.
 */
export function nextSteps(s: RepoStatus, pr: PullRequest | null, review?: string | null): NextStep[] {
  const steps: NextStep[] = [];
  const onBase = onBaseBranch(s);
  if (pr?.checks === "failing" && pr.state === "open") {
    const names = pr.failing.length ? `: ${pr.failing.join(", ")}` : "";
    steps.push({ id: "checks", label: "Fix the checks", message: `Checks failed on PR #${pr.number}${names}. Fix them.` });
  }
  if (s.changes.files > 0) {
    steps.push({ id: "review", label: "Review", message: review ?? "Review your changes so far and tell me what you'd fix." });
  }
  if (s.dirty > 0) steps.push({ id: "commit", label: "Commit", message: "Commit these changes with a clear message." });
  if (!onBase && !pr && (s.commits > 0 || s.dirty > 0)) {
    steps.push({
      id: "pr",
      label: "Open a PR",
      message: s.dirty > 0 ? "Commit these changes, push this branch and open a pull request." : "Push this branch and open a pull request.",
    });
  } else if (s.ahead > 0) {
    steps.push({ id: "push", label: "Push", message: "Push this branch." });
  }
  return steps;
}

/** Files a thread's own edit tool calls touched (absolute or relative paths, as the tools gave them). */
export function matchesEdited(rel: string, edited: Set<string>): boolean {
  if (edited.has(rel)) return true;
  for (const p of edited) if (p.endsWith(`/${rel}`)) return true;
  return false;
}
