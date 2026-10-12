import { Check, ChevronDown, Folder, FolderOpen } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { guyName, sessionsFor } from "../lib/guys";
import { isTauri } from "../lib/ipc";
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

/**
 * Where a new thread will work: the guy's folder, a recent one, or any other.
 * `inline`: inside a labelled form field rather than above the composer.
 */
export function FolderPicker({ guy, value, onChange, inline = false }: { guy: Guy; value: string; onChange: (dir: string) => void; inline?: boolean }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const roster = useApp((s) => s.roster);

  // The guy's folder first, then folders its other threads used, most recent first.
  const choices = useMemo(() => {
    const dirs = sessionsFor(roster, guy)
      .filter((r) => r.workspaceDir)
      .sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0))
      .map((r) => r.workspaceDir as string);
    const all = [guy.workspace, ...dirs, value].filter((d): d is string => Boolean(d));
    return [...new Set(all)].slice(0, MAX_RECENT + 1);
  }, [roster, guy, value]);

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
        <ChevronDown size={12} className="shrink-0" />
      </button>
      {open && (
        <div className={`animate-pop absolute bottom-full ${inline ? "left-0" : "left-5"} z-20 mb-1 w-72 rounded-lg border border-[var(--color-line)] bg-[var(--color-pane-solid)] p-1 text-[12.5px] text-[var(--color-ink)] shadow-xl`}>
          {choices.map((dir) => (
            <button
              key={dir}
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
            </button>
          ))}
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
