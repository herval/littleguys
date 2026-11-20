import { Check, ChevronDown, Folder, FolderOpen, GitBranchPlus, Pin } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { saveCoderPrefs, useCoderPrefs } from "../lib/code";
import { followsSessions, gatewayOf, guyName, sessionsFor } from "../lib/guys";
import { invoke, isTauri } from "../lib/ipc";
import type { Guy } from "../lib/types";
import { useApp } from "../stores/app";

const MAX_RECENT = 6;

export function tildify(path: string): string {
  return path.replace(/^\/Users\/[^/]+/, "~");
}

function baseName(path: string): string {
  const t = tildify(path).replace(/\/+$/, "");
  return t === "~" ? "~" : (t.split("/").pop() ?? t);
}

/** The folder a thread works in, from the sessions list (ACP threads), else the guy's. */
export function threadFolder(guy: Guy, row: { workspaceDir?: string } | undefined): string | null {
  return row?.workspaceDir || guy.workspace || null;
}

/**
 * The folder a thread hasn't started in yet, picked before its first message
 * (kept per thread, like composer drafts).
 */
export function usePendingFolder(threadId: string): [string | null, (dir: string | null) => void] {
  const storageKey = `folder:${threadId}`;
  const [dir, setDir] = useState<string | null>(() => sessionStorage.getItem(storageKey));
  useEffect(() => setDir(sessionStorage.getItem(storageKey)), [storageKey]);
  const set = (next: string | null) => {
    if (next) sessionStorage.setItem(storageKey, next);
    else sessionStorage.removeItem(storageKey);
    setDir(next);
  };
  return [dir, set];
}

/** A thread's first message can start it in a new worktree (`worktree`), for the guy's runtime and this folder. */
export function useCanWorktree(guy: Guy, folder: string | null): boolean {
  const prefs = useCoderPrefs();
  const kind = useApp((s) => gatewayOf(s.roster, guy.gatewayId)?.status.kind);
  const allowed = prefs.on && prefs.worktrees && followsSessions(kind) && Boolean(folder);
  const [repo, setRepo] = useState(false);
  useEffect(() => {
    if (!allowed || !folder) return setRepo(false);
    let live = true;
    invoke<boolean>("in_git_repo", { folder })
      .then((yes) => live && setRepo(yes))
      .catch(() => live && setRepo(false));
    return () => {
      live = false;
    };
  }, [allowed, folder]);
  return allowed && repo;
}

/**
 * Where a new thread will work: coder mode's "Start chats in" folders, the guy's folder, a
 * recent one, or any other. `inline`: inside a labelled form field rather than above the
 * composer. `onWorktree`: the thread can start in a new worktree instead (coder mode).
 */
export function FolderPicker({
  guy,
  value,
  onChange,
  inline = false,
  worktree = false,
  onWorktree,
}: {
  guy: Guy;
  value: string;
  onChange: (dir: string) => void;
  inline?: boolean;
  worktree?: boolean;
  onWorktree?: (on: boolean) => void;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const roster = useApp((s) => s.roster);
  const coder = useCoderPrefs();
  const pinned = useMemo(() => (coder.on ? coder.folders : []), [coder.on, coder.folders]);
  const canWorktree = useCanWorktree(guy, value) && Boolean(onWorktree);

  // The guy's folder first, then folders its other threads used, most recent first.
  const choices = useMemo(() => {
    const dirs = sessionsFor(roster, guy)
      .filter((r) => r.workspaceDir)
      .sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0))
      .map((r) => r.workspaceDir as string);
    const all = [guy.workspace, ...dirs, value].filter((d): d is string => Boolean(d) && !pinned.includes(d as string));
    return [...pinned, ...[...new Set(all)].slice(0, MAX_RECENT + 1)];
  }, [roster, guy, value, pinned]);

  const togglePin = (dir: string) =>
    saveCoderPrefs({ folders: pinned.includes(dir) ? coder.folders.filter((d) => d !== dir) : [...coder.folders, dir] }).catch(() => {});

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  const pick = (dir: string) => {
    setOpen(false);
    onChange(dir);
  };

  const browse = async () => {
    setOpen(false);
    const { open: openDialog } = await import("@tauri-apps/plugin-dialog");
    const dir = await openDialog({ directory: true, multiple: false, defaultPath: value, title: "Where should this thread work?" });
    if (typeof dir === "string") onChange(dir);
  };

  return (
    <div ref={ref} className={`relative ${inline ? "" : "px-5 pb-0.5"}`}>
      <button
        onClick={() => setOpen((o) => !o)}
        title={`Works in ${value}`}
        className="flex max-w-full items-center gap-1.5 rounded-full px-2 py-1 text-[12px] text-[var(--color-ink-3)] transition hover:bg-[var(--color-chip)] hover:text-[var(--color-ink)]"
      >
        <Folder size={12} className="shrink-0" />
        {!inline && <span className="shrink-0">Works in</span>}
        <span className="truncate font-medium text-[var(--color-ink-2)]">{baseName(value)}</span>
        {canWorktree && worktree && <span className="shrink-0">· in a new worktree</span>}
        <ChevronDown size={12} className="shrink-0" />
      </button>
      {open && (
        <div className={`animate-pop absolute bottom-full ${inline ? "left-0" : "left-5"} z-20 mb-1 w-72 rounded-lg border border-[var(--color-line)] bg-[var(--color-pane-solid)] p-1 text-[12.5px] text-[var(--color-ink)] shadow-xl`}>
          {choices.map((dir, i) => (
            <div key={dir} className={`group/row relative ${i > 0 && i === pinned.length ? "mt-1 border-t border-[var(--color-line)] pt-1" : ""}`}>
              <button
                onClick={() => pick(dir)}
                title={dir}
                className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left transition hover:bg-[var(--color-chip)]"
              >
                <Folder size={13} className="shrink-0 opacity-70" />
                <span className="min-w-0 flex-1">
                  <span className="block truncate">
                    {baseName(dir)}
                    {dir === guy.workspace && <span className="ml-1.5 text-[11px] text-[var(--color-ink-3)]">{guyName(guy)}'s folder</span>}
                  </span>
                  <span className="block truncate font-mono text-[10.5px] text-[var(--color-ink-3)]">{tildify(dir)}</span>
                </span>
                {dir === value && <Check size={13} className="shrink-0 text-[var(--color-accent-strong)]" />}
                {coder.on && <span className="w-5 shrink-0" />}
              </button>
              {coder.on && (
                <button
                  onClick={() => togglePin(dir)}
                  title={pinned.includes(dir) ? "Don't always offer this folder" : "Always offer this folder (Settings → Coder mode)"}
                  className={`absolute right-1.5 top-1/2 -translate-y-1/2 rounded p-1 transition hover:bg-black/5 dark:hover:bg-white/10 ${
                    pinned.includes(dir) ? "text-[var(--color-accent-strong)]" : "text-[var(--color-ink-3)] opacity-0 group-hover/row:opacity-100"
                  }`}
                >
                  <Pin size={12} className={pinned.includes(dir) ? "fill-current" : ""} />
                </button>
              )}
            </div>
          ))}
          {canWorktree && onWorktree && (
            <>
              <div className="my-1 border-t border-[var(--color-line)]" />
              <button
                onClick={() => {
                  setOpen(false);
                  onWorktree(!worktree);
                }}
                title="The agent starts on its own branch, in a new folder beside the repo, and your checkout stays as it is."
                className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left transition hover:bg-[var(--color-chip)]"
              >
                <GitBranchPlus size={13} className="shrink-0 opacity-70" />
                <span className="min-w-0 flex-1">In a new worktree</span>
                {worktree && <Check size={13} className="shrink-0 text-[var(--color-accent-strong)]" />}
              </button>
            </>
          )}
          {isTauri && (
            <>
              <div className="my-1 border-t border-[var(--color-line)]" />
              <button
                onClick={browse}
                className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left transition hover:bg-[var(--color-chip)]"
              >
                <FolderOpen size={13} className="shrink-0 opacity-70" />
                Choose another folder…
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
}
