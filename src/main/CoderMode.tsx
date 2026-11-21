import { Folder, FolderPlus, X } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { saveCoderPrefs, useCoderPrefs } from "../lib/code";
import { core, errorMessage, isTauri } from "../lib/ipc";
import { COMPUTER, IS_MAC } from "../lib/platform";
import type { CoderPrefs, TerminalApp } from "../lib/types";
import { IconButton, inputBase, Switch } from "../ui/kit";
import { tildify } from "./FolderPicker";

/** Settings section: coder mode's switch, the folders chats start in, and its parts. */
export function CoderMode() {
  const prefs = useCoderPrefs();
  const [tools, setTools] = useState<{ git: boolean; gh: boolean } | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    core.coderTools().then(setTools).catch(() => setTools({ git: true, gh: false }));
  }, []);

  const save = (patch: Partial<CoderPrefs>) => {
    setError(null);
    saveCoderPrefs(patch).catch((e) => setError(errorMessage(e)));
  };

  const addFolder = async () => {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const dir = await open({ directory: true, multiple: false, title: "Start chats in…" });
    if (typeof dir === "string" && !prefs.folders.includes(dir)) save({ folders: [...prefs.folders, dir] });
  };

  return (
    <section className="mt-5">
      <div className="rounded-xl border border-[var(--color-line)]">
        <SwitchRow
          title="Coder mode"
          hint="Show each coding thread's branch, changes and pull request, and ask the agent to commit or open one."
          on={prefs.on}
          onChange={(on) => save({ on })}
          strong
        />
      </div>
      {tools && !tools.git && (
        <p className="mt-1.5 text-[11.5px] text-[var(--color-warn)]">
          Git isn't installed on this computer, so coder mode has nothing to read yet.
        </p>
      )}

      <div className={prefs.on ? "" : "pointer-events-none opacity-50"} aria-disabled={!prefs.on}>
        <h3 className="mb-1.5 mt-5 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">Start chats in</h3>
        <div className="overflow-hidden rounded-xl border border-[var(--color-line)]">
          {prefs.folders.map((dir) => (
            <div key={dir} className="group flex items-center gap-2.5 border-b border-[var(--color-line)] px-3 py-2 text-[12.5px]">
              <Folder size={14} className="shrink-0 text-[var(--color-ink-3)]" />
              <span className="shrink-0 font-medium">{dir.split("/").filter(Boolean).pop() ?? dir}</span>
              <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-[var(--color-ink-3)]" title={dir}>
                {tildify(dir)}
              </span>
              <IconButton label="Remove" onClick={() => save({ folders: prefs.folders.filter((d) => d !== dir) })}>
                <X size={13} />
              </IconButton>
            </div>
          ))}
          {isTauri ? (
            <button
              onClick={addFolder}
              className="flex w-full items-center gap-2.5 px-3 py-2 text-left text-[12.5px] text-[var(--color-ink-2)] transition hover:bg-[var(--color-chip)]"
            >
              <FolderPlus size={14} className="shrink-0" />
              Add a folder…
            </button>
          ) : (
            prefs.folders.length === 0 && <p className="px-3 py-2 text-[12px] text-[var(--color-ink-3)]">No folders yet.</p>
          )}
        </div>
        <p className="mt-1.5 text-[11px] text-[var(--color-ink-3)]">
          These are always offered when you start a chat. Any folder in a git repo gets coder mode, listed here or not.
        </p>

        <div className="mt-5 rounded-xl border border-[var(--color-line)]">
          <SwitchRow
            title="Show the Changes panel"
            hint="Opens beside a coding thread when it has changes. Off, it opens from the +/− count in the header."
            on={prefs.panel}
            onChange={(panel) => save({ panel })}
          />
          <SwitchRow
            title="Show edits in the chat"
            hint="Each edit shows as a diff between the messages, even for a guy whose steps are hidden. Off, edits are only in the steps."
            on={prefs.edits}
            onChange={(edits) => save({ edits })}
            border
          />
          <SwitchRow
            title="New threads can use their own worktree"
            hint="Adds “In a new worktree” when a Claude Code or Codex thread starts in a repo."
            on={prefs.worktrees}
            onChange={(worktrees) => save({ worktrees })}
            border
          />
          {tools?.gh && (
            <SwitchRow
              title="Show pull requests and checks"
              hint="Reads each branch's pull request with your gh login. The only part of coder mode that goes online."
              on={prefs.prs}
              onChange={(prs) => save({ prs })}
              border
            />
          )}
          <SwitchRow
            title="Open a terminal from coding threads"
            hint="Adds a terminal button that opens the thread's folder."
            on={prefs.terminal}
            onChange={(terminal) => save({ terminal })}
            border
          />
          {prefs.terminal && <TerminalPicker app={prefs.terminalApp} save={save} />}
        </div>
        {prefs.terminal && <TerminalNote app={prefs.terminalApp} />}
      </div>
      {error && <p className="mt-1.5 text-[11.5px] text-[var(--color-bad)]">{error}</p>}
    </section>
  );
}

const CHOOSE = "\u0000choose";

function useTerminals(app: string | null): TerminalApp[] | null {
  const [list, setList] = useState<TerminalApp[] | null>(null);
  // Asked again after a change, so an app picked by hand joins the list.
  useEffect(() => {
    core.coderTerminals().then(setList).catch(() => setList([]));
  }, [app]);
  return list;
}

/** "Open it in ▾": the terminals here, the system's first, and any other app on a Mac. */
function TerminalPicker({ app, save }: { app: string | null; save: (patch: Partial<CoderPrefs>) => void }) {
  const list = useTerminals(app);
  if (!list?.length) return null;
  const system = list.find((t) => t.system) ?? list[0];
  const current = (app && list.find((t) => t.app === app)) || system;

  const choose = async (value: string) => {
    if (value !== CHOOSE) return save({ terminalApp: value === system.app ? null : value });
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({ multiple: false, directory: false, defaultPath: "/Applications", filters: [{ name: "Apps", extensions: ["app"] }], title: "Open terminals in…" });
    if (typeof picked === "string") save({ terminalApp: picked });
  };

  return (
    <label className="flex items-center gap-3 border-t border-[var(--color-line)] px-3.5 py-2">
      <span className="min-w-0 flex-1 text-[12px] text-[var(--color-ink-2)]">Open it in</span>
      <select className={`${inputBase} w-auto`} value={current.app} onChange={(e) => choose(e.target.value)}>
        {list.map((t) => (
          <option key={t.app} value={t.app}>
            {t.name}
            {t.system ? " (default)" : ""}
          </option>
        ))}
        {IS_MAC && isTauri && <option value={CHOOSE}>Choose another app…</option>}
      </select>
    </label>
  );
}

/** Under the picker: when the chosen terminal may not start in the folder, or is gone. */
function TerminalNote({ app }: { app: string | null }) {
  const list = useTerminals(app);
  if (!list?.length || !app) {
    const system = list?.find((t) => t.system);
    return system && !system.folders ? <Warn>{system.name} doesn't say it opens folders, so it may start in your home folder.</Warn> : null;
  }
  const current = list.find((t) => t.app === app);
  const system = list.find((t) => t.system) ?? list[0];
  if (!current) {
    const name = app.split(/[\\/]/).pop()?.replace(/\.app$/, "") ?? app;
    return <Warn>{name} isn't on this {COMPUTER} anymore, so {system.name} opens instead.</Warn>;
  }
  return current.folders ? null : <Warn>{current.name} doesn't say it opens folders, so it may start in your home folder.</Warn>;
}

function Warn({ children }: { children: ReactNode }) {
  return <p className="mt-1.5 text-[11.5px] text-[var(--color-warn)]">{children}</p>;
}

function SwitchRow({
  title,
  hint,
  on,
  onChange,
  strong = false,
  border = false,
}: {
  title: ReactNode;
  hint: ReactNode;
  on: boolean;
  onChange: (on: boolean) => void;
  strong?: boolean;
  border?: boolean;
}) {
  return (
    <label className={`flex cursor-default items-start gap-3 px-3.5 py-2.5 ${border ? "border-t border-[var(--color-line)]" : ""}`}>
      <span className="min-w-0 flex-1">
        <span className={`block text-[12.5px] ${strong ? "font-semibold" : "font-medium"}`}>{title}</span>
        <span className="mt-0.5 block text-[11.5px] leading-snug text-[var(--color-ink-3)]">{hint}</span>
      </span>
      <Switch on={on} onChange={onChange} label={typeof title === "string" ? title : undefined} className="mt-0.5" />
    </label>
  );
}
