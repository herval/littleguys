import { Folder, History, RotateCw, Search } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { COMPUTER } from "../lib/platform";
import { sessions as sessionsApi } from "../lib/gateway";
import { guyName, relativeTime } from "../lib/guys";
import { core, errorMessage } from "../lib/ipc";
import type { Guy, PastSession } from "../lib/types";
import { useApp } from "../stores/app";
import { Button, IconButton, inputClass, Spinner } from "../ui/kit";
import { tildify } from "./FolderPicker";

const PER_FOLDER = 5;
/** Touched this recently, a session may still be open in a terminal. */
const ACTIVE_MS = 10 * 60_000;

/**
 * Runs nobody typed: tools that call Claude headless from `/`, and the agent's
 * own title generation in temporary folders.
 */
function isBackground(cwd: string): boolean {
  return cwd === "/" || /^\/(private\/)?(var\/folders|tmp)\//.test(cwd);
}

/**
 * The resume picker: every session the guy's agent can resume, from every
 * folder (like `claude --resume`). Opening one makes it a thread that works in
 * its own folder.
 */
export function PastSessions({ guy }: { guy: Guy }) {
  const gw = guy.gatewayId;
  const selectGuy = useApp((s) => s.selectGuy);
  const [list, setList] = useState<PastSession[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [showBackground, setShowBackground] = useState(false);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [opening, setOpening] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      setList((await sessionsApi.past(gw)).sessions);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, [gw]);
  useEffect(() => {
    load();
  }, [load]);

  const q = query.trim().toLowerCase();
  const background = useMemo(() => (list ?? []).filter((s) => isBackground(s.cwd)).length, [list]);
  // Newest first, so each folder lands where its newest session is.
  const folders = useMemo(() => {
    const byCwd = new Map<string, PastSession[]>();
    for (const s of list ?? []) {
      // A search looks everywhere; otherwise background runs stay out of the way.
      const hidden = q ? ![s.title ?? "", s.cwd].some((v) => v.toLowerCase().includes(q)) : !showBackground && isBackground(s.cwd);
      if (hidden) continue;
      byCwd.set(s.cwd, [...(byCwd.get(s.cwd) ?? []), s]);
    }
    return [...byCwd].map(([cwd, sessions]) => ({ cwd, sessions }));
  }, [list, q, showBackground]);

  const open = async (s: PastSession) => {
    if (opening) return;
    setOpening(s.sessionId);
    setError(null);
    try {
      const { key } = await sessionsApi.open(gw, { sessionId: s.sessionId, cwd: s.cwd, title: s.title, updatedAt: s.updatedAt });
      await core.refreshRoster(gw).catch(() => {});
      selectGuy(guy.uid, key);
    } catch (e) {
      setError(errorMessage(e));
      setOpening(null);
    }
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div data-tauri-drag-region className="flex h-14 shrink-0 items-center gap-3 border-b border-[var(--color-line)] px-5">
        <span className="flex h-[34px] w-[34px] shrink-0 items-center justify-center rounded-full bg-[var(--color-chip)] text-[var(--color-ink-2)]">
          <History size={16} />
        </span>
        <div data-tauri-drag-region className="min-w-0 flex-1">
          <h3 className="font-display text-[14.5px] font-semibold leading-tight">Resume a session</h3>
          <p className="mt-0.5 truncate text-[11.5px] leading-tight text-[var(--color-ink-3)]">
            {guyName(guy)}'s sessions from every folder on this {COMPUTER}, including ones from a terminal
          </p>
        </div>
        <IconButton
          label="Look again"
          onClick={() => {
            setList(null);
            load();
          }}
        >
          <RotateCw size={13} />
        </IconButton>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-5 pb-6">
        <div className="mx-auto max-w-[720px]">
          <div className="sticky top-0 z-10 bg-[var(--color-pane-solid)] pb-2 pt-3">
            <div className="relative">
              <Search size={13} className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-[var(--color-ink-3)]" />
              <input
                autoFocus
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                onKeyDown={(e) => {
                  const first = folders[0]?.sessions[0];
                  if (e.key === "Enter" && first) open(first);
                }}
                placeholder="Search by title or folder"
                className={`${inputClass} pl-8`}
              />
            </div>
          </div>
          {error && <p className="my-2 rounded-lg bg-[var(--color-bad)]/10 px-3 py-2 text-[12px] text-[var(--color-bad)]">{error}</p>}
          {!list && !error && (
            <div className="flex items-center justify-center gap-2 py-16 text-[12.5px] text-[var(--color-ink-3)]">
              <Spinner size={14} /> Looking through every folder…
            </div>
          )}
          {!list && error && (
            <div className="py-6 text-center">
              <Button onClick={load}>
                <RotateCw size={13} /> Try again
              </Button>
            </div>
          )}
          {list && folders.length === 0 && (
            <p className="py-16 text-center text-[12.5px] text-[var(--color-ink-3)]">
              {q ? "No session matches that." : `No sessions to resume yet. Ones you start with ${guyName(guy)}, here or in a terminal, show up here.`}
            </p>
          )}
          {folders.map(({ cwd, sessions }) => {
            const all = Boolean(q) || expanded[cwd];
            const shown = all ? sessions : sessions.slice(0, PER_FOLDER);
            return (
              <section key={cwd} className="mt-3">
                <h4 className="flex items-center gap-1.5 px-3 pb-1 text-[11.5px] font-medium text-[var(--color-ink-3)]" title={cwd}>
                  <Folder size={12} className="shrink-0" />
                  <span className="truncate font-mono text-[11px]">{tildify(cwd)}</span>
                  <span className="shrink-0 rounded-full bg-[var(--color-chip)] px-1.5 text-[10.5px] tabular-nums">{sessions.length}</span>
                </h4>
                {shown.map((s) => (
                  <SessionRow key={s.sessionId} session={s} opening={opening === s.sessionId} disabled={Boolean(opening)} onOpen={() => open(s)} />
                ))}
                {!all && sessions.length > PER_FOLDER && (
                  <button
                    onClick={() => setExpanded((x) => ({ ...x, [cwd]: true }))}
                    className="px-3 py-1 text-[12px] font-medium text-[var(--color-accent-strong)] hover:underline"
                  >
                    Show {sessions.length - PER_FOLDER} more
                  </button>
                )}
              </section>
            );
          })}
          {list && !q && background > 0 && (
            <p className="mt-6 text-center text-[11.5px] text-[var(--color-ink-3)]">
              {showBackground ? "Showing" : "Hiding"} {background} background {background === 1 ? "run" : "runs"} (tools calling Claude from / or a
              temporary folder).{" "}
              <button onClick={() => setShowBackground((b) => !b)} className="font-medium text-[var(--color-accent-strong)] hover:underline">
                {showBackground ? "Hide them" : "Show them"}
              </button>
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

function SessionRow({ session: s, opening, disabled, onOpen }: { session: PastSession; opening: boolean; disabled: boolean; onOpen: () => void }) {
  const active = Boolean(s.updatedAt && Date.now() - s.updatedAt < ACTIVE_MS);
  return (
    <button
      onClick={onOpen}
      disabled={disabled}
      className="flex w-full items-center gap-3 rounded-lg px-3 py-2 text-left transition enabled:hover:bg-[var(--color-chip)] disabled:cursor-default"
    >
      <span className="min-w-0 flex-1">
        <span className="block truncate text-[13px] font-medium">{s.title || "Untitled session"}</span>
        {active && (
          <span className="mt-0.5 flex items-center gap-1.5 text-[11px] text-[var(--color-warn)]">
            <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-[var(--color-warn)]" />
            Active {s.updatedAt ? relativeTime(s.updatedAt) : ""}, may still be open in a terminal
          </span>
        )}
      </span>
      {s.inThreads && (
        <span className="shrink-0 rounded-full bg-[var(--color-chip)] px-2 py-0.5 text-[10.5px] text-[var(--color-ink-3)]">in your threads</span>
      )}
      <span className="w-16 shrink-0 text-right text-[11px] text-[var(--color-ink-3)]">
        {opening ? <Spinner size={12} /> : s.updatedAt ? relativeTime(s.updatedAt) : ""}
      </span>
    </button>
  );
}
