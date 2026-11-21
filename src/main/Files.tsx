// The Files tab: a guy's workspace folder. The tree lives in the sidebar
// (`FileTree`), the open file in the main pane (`FilesView`). Text files can be
// edited where the runtime can save them (`editFiles`: guys on this computer), and
// an OpenClaw guy's own files (SOUL.md, AGENTS.md…) through OpenClaw's file API.
import {
  Activity,
  Brain,
  ChevronLeft,
  ChevronRight,
  Copy,
  File,
  FileCode2,
  FileImage,
  FileText,
  Folder,
  FolderOpen,
  Heart,
  IdCard,
  type LucideIcon,
  Moon,
  Pencil,
  RotateCw,
  ScrollText,
  Sparkles,
  UserRound,
  Wrench,
  X,
} from "lucide-react";
import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { readEditableFile } from "../lib/fileEdit";
import { agents, workspace } from "../lib/gateway";
import { capsOf, findGuy, followsSessions, gatewayOf, guyHue, guyName, hasMainChat, isOnThisMac, sessionsFor, shortTime, threadKind, threadTitle } from "../lib/guys";
import { core, errorMessage, type RpcError } from "../lib/ipc";
import { FILE_MANAGER, shortcut } from "../lib/platform";
import type { Guy, WorkspaceEntry, WorkspaceFile } from "../lib/types";
import { useApp } from "../stores/app";
import { Markdown } from "../ui/Markdown";
import { confirmAction } from "../ui/confirm";
import { Button, IconButton, Segmented, Spinner } from "../ui/kit";
import { threadFolder } from "./FolderPicker";

// CodeMirror loads the first time you edit a file.
const CodeEditor = lazy(() => import("../ui/CodeEditor"));

type Dir = { entries: WorkspaceEntry[]; total: number; loading: boolean; error?: string };

/** The files that make a guy who they are, by what they mean rather than their names. */
const CORE_FILES: Array<{ name: string; label: string; icon: LucideIcon }> = [
  { name: "SOUL.md", label: "Personality", icon: Heart },
  { name: "AGENTS.md", label: "Instructions", icon: ScrollText },
  { name: "CLAUDE.md", label: "Instructions", icon: ScrollText },
  { name: "IDENTITY.md", label: "Identity", icon: IdCard },
  { name: "USER.md", label: "About you", icon: UserRound },
  { name: "MEMORY.md", label: "Memory", icon: Brain },
  { name: "TOOLS.md", label: "Tool notes", icon: Wrench },
  { name: "HEARTBEAT.md", label: "Heartbeat", icon: Activity },
  { name: "DREAMS.md", label: "Dreams", icon: Moon },
  { name: "BOOTSTRAP.md", label: "First run", icon: Sparkles },
];
const coreFile = (path: string) => (path.includes("/") ? undefined : CORE_FILES.find((c) => c.name === path));
/** The files OpenClaw's `agents.files.set` writes; its other workspace files stay read-only here. */
const OPENCLAW_EDITABLE = new Set(["AGENTS.md", "SOUL.md", "TOOLS.md", "IDENTITY.md", "USER.md", "BOOTSTRAP.md", "MEMORY.md"]);

const CODE_LANG: Record<string, string> = {
  ts: "typescript", tsx: "typescript", js: "javascript", jsx: "javascript", mjs: "javascript", cjs: "javascript",
  json: "json", jsonl: "json", json5: "json", py: "python", rs: "rust", go: "go", rb: "ruby", java: "java",
  kt: "kotlin", swift: "swift", c: "c", h: "c", cpp: "cpp", cc: "cpp", cs: "csharp", php: "php",
  sh: "bash", bash: "bash", zsh: "bash", fish: "bash", yml: "yaml", yaml: "yaml", toml: "ini", ini: "ini",
  css: "css", scss: "scss", html: "xml", xml: "xml", svg: "xml", sql: "sql", lua: "lua", diff: "diff",
};
const IMAGE_EXT = new Set(["png", "jpg", "jpeg", "gif", "webp", "bmp", "avif", "heic", "heif"]);

const extOf = (name: string) => (name.includes(".") ? name.split(".").pop()!.toLowerCase() : "");
const isMarkdown = (name: string) => ["md", "markdown", "mdx"].includes(extOf(name));

function formatBytes(n?: number): string {
  if (n == null) return "";
  if (n < 1024) return `${n} bytes`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

function EntryIcon({ entry, open }: { entry: WorkspaceEntry; open?: boolean }) {
  if (entry.kind === "directory") {
    const Icon = open ? FolderOpen : Folder;
    return <Icon size={14} className="shrink-0 text-[#e0a13a]" />;
  }
  const ext = extOf(entry.name);
  if (IMAGE_EXT.has(ext)) return <FileImage size={14} className="shrink-0 text-[#a77be0]" />;
  if (isMarkdown(entry.name) || ext === "txt") return <FileText size={14} className="shrink-0 text-[var(--guy-ink)]" />;
  if (CODE_LANG[ext]) return <FileCode2 size={14} className="shrink-0 text-[#4c9be8]" />;
  return <File size={14} className="shrink-0 text-[var(--color-ink-3)]" />;
}

/**
 * The thread whose folder the Files tab shows, for bots whose threads each work in their
 * own (ACP on this Mac, Claude Code, Codex): the one open, else the one you were on last.
 */
function useFilesThread(guy: Guy): string | undefined {
  return useApp((s) => {
    const g = gatewayOf(s.roster, guy.gatewayId);
    if (!followsSessions(g?.status.kind) && !(g?.status.kind === "acp" && isOnThisMac(g))) return undefined;
    return (s.selectedUid === guy.uid ? s.selectedSessionKey : null) ?? s.lastThreadByGuy[guy.uid] ?? undefined;
  });
}

/** The folder the Files tab shows: the thread's, else the guy's. */
function useFilesFolder(guy: Guy, thread: string | undefined): string | null {
  return useApp((s) => (thread ? threadFolder(guy, sessionsFor(s.roster, guy).find((r) => r.key === thread)) : guy.workspace) ?? null);
}

/** The file you last opened in each folder, by `${guy uid}|${folder}`. */
const openedIn = new Map<string, string>();

/** The workspace tree, in the sidebar. Folders load as you open them. */
export function FileTree({ guy }: { guy: Guy }) {
  const thread = useFilesThread(guy);
  const folder = useFilesFolder(guy, thread);
  // Another thread's folder is another tree.
  return <Tree key={folder ?? ""} guy={guy} thread={thread} folder={folder} />;
}

function Tree({ guy, thread, folder }: { guy: Guy; thread?: string; folder: string | null }) {
  // Highlight only what's on screen.
  const selected = useApp((s) => (s.selectedUid === guy.uid && s.view === "files" ? s.fileByGuy[guy.uid] : undefined));
  const pick = useApp((s) => s.selectFile);
  const remembered = `${guy.uid}|${folder ?? ""}`;
  const selectFile = useCallback(
    (path: string) => {
      openedIn.set(remembered, path);
      pick(guy.uid, path);
    },
    [pick, guy.uid, remembered],
  );
  const gateway = useApp((s) => gatewayOf(s.roster, guy.gatewayId));
  const [pathNote, setPathNote] = useState<string | null>(null);
  const [dirs, setDirs] = useState<Record<string, Dir>>({});
  const [open, setOpen] = useState<Set<string>>(() => new Set([""]));
  // The folder the listing says it's of (exact even for threads not in the roster).
  const [listed, setListed] = useState<string | null>(null);

  const load = useCallback(
    async (path: string, offset = 0) => {
      setDirs((d) => ({ ...d, [path]: { entries: d[path]?.entries ?? [], total: d[path]?.total ?? 0, loading: true } }));
      try {
        const res = await workspace.list(guy.gatewayId, guy.id, path, offset, thread);
        if (!path && res.workspaceDir) setListed(res.workspaceDir);
        setDirs((d) => ({
          ...d,
          [path]: { entries: offset ? [...(d[path]?.entries ?? []), ...res.entries] : res.entries, total: res.totalEntries, loading: false },
        }));
        return res.entries;
      } catch (e) {
        setDirs((d) => ({ ...d, [path]: { entries: d[path]?.entries ?? [], total: 0, loading: false, error: errorMessage(e) } }));
        return [];
      }
    },
    [guy.gatewayId, guy.id, thread],
  );

  // Root on mount. Open the file you last had open in this folder, else the guy's
  // personality (or the first file); remembered for when the Files view opens.
  useEffect(() => {
    load("").then((entries) => {
      const before = openedIn.get(remembered);
      if (before) return pick(guy.uid, before, false);
      const current = useApp.getState().fileByGuy[guy.uid];
      if (current && !thread) return openedIn.set(remembered, current);
      const files = entries.filter((e) => e.kind === "file");
      const first = ["SOUL.md", "AGENTS.md", "CLAUDE.md", "README.md"].map((n) => files.find((f) => f.name === n)).find(Boolean) ?? files[0];
      if (first) {
        openedIn.set(remembered, first.path);
        pick(guy.uid, first.path, false);
      }
    });
  }, [load, guy.uid, pick, remembered, thread]);

  const toggle = (path: string) => {
    const next = new Set(open);
    if (next.has(path)) next.delete(path);
    else {
      next.add(path);
      if (!dirs[path]) load(path);
    }
    setOpen(next);
  };

  const refresh = () => {
    setDirs({});
    for (const path of open) load(path);
  };

  const root = dirs[""];
  const coreFiles = CORE_FILES.filter((c) => root?.entries.some((e) => e.kind === "file" && e.name === c.name));
  // A core file open above isn't highlighted again in the full list.
  const shownAbove = coreFiles.some((c) => c.name === selected);

  const row = (selectedRow: boolean) =>
    `group flex w-full items-center gap-2 rounded-lg py-[5px] pr-2 text-left text-[12.5px] transition-colors ${
      selectedRow ? "bg-[var(--guy-wash-strong)] font-medium text-[var(--color-ink)]" : "text-[var(--color-ink)] hover:bg-[var(--color-chip)]"
    }`;

  const renderDir = (path: string, depth: number): React.ReactNode => {
    const dir = dirs[path];
    if (!dir) return null;
    const inner = (
      <>
        {dir.entries.map((entry) => {
          const isOpen = open.has(entry.path);
          const isDir = entry.kind === "directory";
          return (
            <div key={entry.path}>
              <button
                onClick={() => (isDir ? toggle(entry.path) : selectFile(entry.path))}
                title={entry.path}
                className={`${row(!isDir && !shownAbove && selected === entry.path)} pl-1.5`}
              >
                {isDir ? (
                  <ChevronRight size={11} className={`shrink-0 text-[var(--color-ink-3)] transition-transform ${isOpen ? "rotate-90" : ""}`} />
                ) : (
                  <span className="w-[11px] shrink-0" />
                )}
                <EntryIcon entry={entry} open={isOpen} />
                <span className="min-w-0 flex-1 truncate">{entry.name}</span>
                {!isDir && entry.size != null && (
                  <span className="shrink-0 text-[10.5px] text-[var(--color-ink-3)] opacity-0 transition-opacity group-hover:opacity-100">
                    {formatBytes(entry.size)}
                  </span>
                )}
              </button>
              {isDir && isOpen && renderDir(entry.path, depth + 1)}
            </div>
          );
        })}
        {dir.loading && (
          <div className="flex items-center gap-2 py-1.5 pl-7 text-[11.5px] text-[var(--color-ink-3)]">
            <Spinner size={11} /> Loading
          </div>
        )}
        {dir.error && <p className="py-1 pl-7 pr-2 text-[11.5px] text-[var(--color-bad)]">{dir.error}</p>}
        {!dir.loading && dir.entries.length < dir.total && (
          <button onClick={() => load(path, dir.entries.length)} className="py-1 pl-7 text-[11.5px] font-medium text-[var(--guy-ink)] hover:underline">
            Show {Math.min(250, dir.total - dir.entries.length)} more
          </button>
        )}
        {!dir.loading && !dir.error && dir.total === 0 && (
          <p className="py-1 pl-7 text-[11.5px] text-[var(--color-ink-3)]">{depth === 0 ? "Nothing here yet." : "Empty folder"}</p>
        )}
      </>
    );
    // Nested folders hang off a thin guide line, so depth reads at a glance.
    return depth === 0 ? inner : <div className="ml-[13px] border-l border-[var(--color-line)] pl-1">{inner}</div>;
  };

  return (
    <div className="guy-themed flex min-h-0 flex-1 flex-col" style={{ ["--guy-h" as string]: guyHue(guy.id) }}>
      <div className="flex items-center justify-between pl-4 pr-2.5">
        <h3 className="font-display text-[13px] font-semibold">Workspace</h3>
        <IconButton label="Refresh" onClick={refresh}>
          <RotateCw size={12} />
        </IconButton>
      </div>
      {/* Hermes browses its working folder, not the profile folder it reports as the workspace. */}
      {(listed ?? folder) && gateway?.status.kind !== "hermes" && (
        <WorkspacePath path={(listed ?? folder) as string} local={isOnThisMac(gateway)} gatewayName={guy.gatewayName} note={pathNote} setNote={setPathNote} />
      )}
      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        {coreFiles.length > 0 && (
          <div className="mb-3">
            <p className="px-2 pb-1 text-[11.5px] font-medium text-[var(--color-ink-3)]">Who {guyName(guy)} is</p>
            {coreFiles.map((c) => {
              const Icon = c.icon;
              const active = selected === c.name;
              return (
                <button key={c.name} onClick={() => selectFile(c.name)} className={`${row(active)} pl-2`}>
                  <span className="flex h-5 w-5 shrink-0 items-center justify-center rounded-md bg-[var(--guy-wash-strong)] text-[var(--guy-ink)]">
                    <Icon size={11.5} strokeWidth={2.2} />
                  </span>
                  <span className="min-w-0 flex-1 truncate">{c.label}</span>
                  <span className="shrink-0 text-[10.5px] text-[var(--color-ink-3)]">{c.name}</span>
                </button>
              );
            })}
          </div>
        )}
        <p className="px-2 pb-1 text-[11.5px] font-medium text-[var(--color-ink-3)]">All files</p>
        {renderDir("", 0)}
      </div>
    </div>
  );
}

function prettyPath(p: string): string {
  return p.replace(/^\/Users\/[^/]+/, "~").replace(/^\/home\/[^/]+/, "~");
}

/** Where the workspace lives: opens in Finder on this Mac, copies the path otherwise. */
function WorkspacePath({
  path,
  local,
  gatewayName,
  note,
  setNote,
}: {
  path: string;
  local: boolean;
  gatewayName: string;
  note: string | null;
  setNote: (n: string | null) => void;
}) {
  const flash = (text: string) => {
    setNote(text);
    setTimeout(() => setNote(null), 2500);
  };
  return (
    <button
      onClick={() => {
        if (!local) {
          navigator.clipboard.writeText(path);
          return flash(`Path copied. This folder is on ${gatewayName}.`);
        }
        core.revealPath(path).catch((e) => flash(errorMessage(e)));
      }}
      title={local ? `Show ${path} in ${FILE_MANAGER}` : `${path} on ${gatewayName}. Click to copy.`}
      className="mx-2 mb-2 flex items-center gap-1.5 rounded-md px-2 py-1 text-left text-[11.5px] text-[var(--color-ink-2)] transition hover:bg-[var(--color-chip)]"
    >
      <FolderOpen size={13} className="shrink-0 opacity-70" />
      <span className="truncate">{note ?? prettyPath(path)}</span>
    </button>
  );
}

/** Gutter + highlighted code, like an editor (no wrapping). */
function CodeView({ content, lang }: { content: string; lang: string }) {
  const lines = content.replace(/\n$/, "").split("\n").length;
  const longest = Math.max(2, ...Array.from(content.matchAll(/`+/g), (m) => m[0].length));
  const fence = "`".repeat(longest + 1);
  return (
    <div className="code-view selectable flex min-h-full gap-4 overflow-x-auto px-4 py-4">
      <div className="gutter shrink-0" aria-hidden>
        {Array.from({ length: lines }, (_, i) => (
          <div key={i}>{i + 1}</div>
        ))}
      </div>
      <div className="min-w-0 flex-1">
        <Markdown text={`${fence}${lang}\n${content}\n${fence}`} />
      </div>
    </div>
  );
}

/**
 * A file from a guy's folder over the conversation, as Files shows it,
 * for a path the guy mentioned. Esc or a click beside it closes it.
 */
export function FilePeek({ guy, path, onClose }: { guy: Guy; path: string; onClose: () => void }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // In the editor, Esc stops editing; the next one closes the peek.
      if (e.key === "Escape" && !(e.target instanceof Element && e.target.closest(".cm-editor"))) {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  return createPortal(
    <div role="dialog" aria-modal="true" aria-label={path} onClick={onClose} className="animate-fade fixed inset-0 z-50 flex justify-end bg-black/20 motion-reduce:animate-none">
      <div
        onClick={(e) => e.stopPropagation()}
        className="animate-push h-full w-[min(760px,100%)] border-l border-[var(--color-line)] bg-[var(--color-pane-solid)] shadow-[-12px_0_40px_rgb(0_0_0/0.14)] motion-reduce:animate-none"
      >
        <FilesView guy={guy} path={path} onClose={onClose} />
      </div>
    </div>,
    document.body,
  );
}

/** A file being edited: what it started from, and what you've typed. */
interface EditSession {
  /** `workspace`: `agents.workspace.set` (guys on this computer); `core`: OpenClaw's `agents.files.set`. */
  via: "workspace" | "core";
  /** The text the edit started from, and its version: a save goes only over that one. */
  base: string;
  hash: string;
  /** OpenClaw said the file isn't there yet (a save creates it). */
  missing?: boolean;
  draft: string;
  saving: boolean;
  /** A save met a newer version (the guy edited it meanwhile): that version's hash. */
  conflict: string | null;
  /** That save was Save (leave the editor), not ⌘S: so is Keep mine. */
  conflictExits?: boolean;
  error: string | null;
}

/** Edits not saved yet, by `${guy uid}|${folder}|${path}`, so opening another file keeps them. */
const drafts = new Map<string, EditSession>();

/** Where a file's back button goes, as its tooltip says it: the thread's title, or the tab. */
function backLabel(s: ReturnType<typeof useApp.getState>): string {
  const back = s.fileBack ?? (s.selectedUid ? { uid: s.selectedUid, sessionKey: s.selectedSessionKey, view: "chat" as const } : null);
  if (back?.view === "routines") return "Routines";
  if (back?.view === "skills") return "Skills";
  if (back?.view === "past") return "past sessions";
  if (!back?.sessionKey) return "the chat";
  const gatewayId = back.uid.slice(0, back.uid.indexOf("/"));
  const row = gatewayOf(s.roster, gatewayId)?.sessions.find((r) => r.key === back.sessionKey);
  // Another guy's chat: say whose ("Back to Jefe").
  const other = back.uid !== s.selectedUid ? findGuy(s.roster, back.uid) : null;
  if (other && (!row || threadKind(row.key) === "main")) return guyName(other);
  return row ? threadTitle(row, hasMainChat(s.roster, gatewayId)) : "the chat";
}

/** The open file, in the main pane; with `path` and `onClose`, the one a `FilePeek` shows. */
export function FilesView({ guy, path: shown, onClose }: { guy: Guy; path?: string; onClose?: () => void }) {
  const picked = useApp((s) => s.fileByGuy[guy.uid]);
  const path = shown ?? picked;
  const thread = useFilesThread(guy);
  const folder = useFilesFolder(guy, thread);
  const gateway = useApp((s) => gatewayOf(s.roster, guy.gatewayId));
  const backTo = useApp(backLabel);
  const leaveFile = useApp((s) => s.leaveFile);
  const caps = useApp((s) => capsOf(s.roster, guy.gatewayId));
  const [file, setFile] = useState<WorkspaceFile | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [mode, setMode] = useState<"read" | "source">("read");
  const [copied, setCopied] = useState(false);
  const [nonce, setNonce] = useState(0);
  const [size, setSize] = useState<{ w: number; h: number } | null>(null);

  useEffect(() => {
    if (!path) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    setSize(null);
    workspace
      .get(guy.gatewayId, guy.id, path, thread)
      .then((r) => !cancelled && setFile(r.file))
      .catch((e) => {
        if (cancelled) return;
        setFile(null);
        setError(errorMessage(e));
      })
      .finally(() => !cancelled && setLoading(false));
    return () => {
      cancelled = true;
    };
  }, [guy.gatewayId, guy.id, path, nonce, thread]);

  // ---- editing ----
  const draftKey = path ? `${guy.uid}|${folder ?? ""}|${path}` : "";
  const keyRef = useRef(draftKey);
  keyRef.current = draftKey;
  const [edit, setEditState] = useState<EditSession | null>(() => drafts.get(draftKey) ?? null);
  useEffect(() => {
    setEditState(drafts.get(draftKey) ?? null);
    // Left without changing anything: nothing to come back to.
    return () => {
      const left = drafts.get(draftKey);
      if (left && left.draft === left.base && !left.saving) drafts.delete(draftKey);
    };
  }, [draftKey]);
  /** Set a file's edit; async work names the file it started on, in case you've moved on. */
  const putEdit = useCallback((key: string, next: EditSession | null) => {
    if (next) drafts.set(key, next);
    else drafts.delete(key);
    if (key === keyRef.current) setEditState(next);
  }, []);
  const editVia: EditSession["via"] | null =
    !path || !file || file.encoding !== "utf8" || file.truncated
      ? null
      : caps.editFiles && file.hash
        ? "workspace"
        : caps.manageGuys && gateway?.status.kind === "openclaw" && OPENCLAW_EDITABLE.has(path)
          ? "core"
          : null;
  const [startError, setStartError] = useState<string | null>(null);
  useEffect(() => setStartError(null), [draftKey]);

  const startEdit = async () => {
    if (!file || !editVia || !path) return;
    const key = draftKey;
    const fresh = { saving: false, conflict: null, error: null };
    if (editVia === "workspace") return putEdit(key, { via: "workspace", base: file.content, hash: file.hash!, draft: file.content, ...fresh });
    // OpenClaw's file API has its own version of the file.
    try {
      const read = await readEditableFile(guy.gatewayId, guy.id, path);
      putEdit(key, { via: "core", base: read.content, hash: read.hash ?? "", missing: read.missing, draft: read.content, ...fresh });
    } catch (e) {
      if (key === keyRef.current) setStartError(errorMessage(e));
    }
  };

  /** Save what you typed; `exit` goes back to reading it, `over` overwrites the newer version with that hash. */
  const save = async (exit: boolean, over?: string) => {
    const key = draftKey;
    const at = drafts.get(key);
    if (!at || at.saving || !path) return;
    if (at.draft === at.base && !over) return exit ? putEdit(key, null) : undefined;
    const sent = at.draft;
    putEdit(key, { ...at, saving: true, error: null });
    const latest = () => drafts.get(key) ?? at;
    try {
      let hash: string;
      let saved: Partial<WorkspaceFile> = {};
      if (at.via === "workspace") {
        const r = await workspace.set(guy.gatewayId, guy.id, path, sent, over ?? at.hash, thread);
        ({ hash } = r.file);
        saved = { size: r.file.size, updatedAtMs: r.file.updatedAtMs };
      } else {
        await agents.setFile(guy.gatewayId, guy.id, path, sent, over ?? at.hash, at.missing && !over);
        // OpenClaw doesn't answer with the new version: read it.
        const read = await readEditableFile(guy.gatewayId, guy.id, path);
        hash = read.hash ?? "";
        saved = { size: new TextEncoder().encode(sent).length, updatedAtMs: Date.now() };
      }
      if (key === keyRef.current) setFile((f) => f && { ...f, ...saved, content: sent, hash, truncated: false });
      const now = latest();
      // Typing went on while it saved: keep editing, from the version just saved.
      putEdit(key, exit && now.draft === sent ? null : { ...now, base: sent, hash, missing: false, saving: false, conflict: null, error: null });
    } catch (e) {
      const details = (e as RpcError)?.details as { type?: string; hash?: string } | undefined;
      const now = latest();
      putEdit(
        key,
        details?.type === "workspace_file_changed" && details.hash
          ? { ...now, saving: false, conflict: details.hash, conflictExits: exit }
          : { ...now, saving: false, error: errorMessage(e) },
      );
    }
  };

  const cancelEdit = async () => {
    const key = draftKey;
    const at = drafts.get(key);
    if (!at) return;
    if (at.draft !== at.base && !(await confirmAction(`Discard your changes to ${path?.split("/").pop()}?`, "littleguys", "Discard"))) return;
    putEdit(key, null);
  };

  /** Drop what you typed for the guy's newer version. */
  const takeTheirs = () => {
    putEdit(draftKey, null);
    setNonce((n) => n + 1);
  };

  // Hermes roots the files at its working folder, not the guy's profile folder.
  const absolute = path && folder && gateway?.status.kind !== "hermes" ? `${folder.replace(/\/$/, "")}/${path}` : null;
  const local = isOnThisMac(gateway);

  const body = useMemo(() => {
    if (!file) return null;
    if (file.encoding === "base64") {
      return (
        <div className="flex min-h-full flex-col items-center justify-center gap-3 bg-[repeating-conic-gradient(var(--color-chip)_0%_25%,transparent_0%_50%)] bg-[length:18px_18px] p-10">
          <img
            src={`data:${file.mimeType};base64,${file.content}`}
            alt={file.name}
            onLoad={(e) => setSize({ w: e.currentTarget.naturalWidth, h: e.currentTarget.naturalHeight })}
            className="max-h-[70vh] max-w-full rounded-md shadow-[0_8px_30px_rgb(0_0_0/0.12)]"
            style={{ imageRendering: size && size.w < 64 ? "pixelated" : undefined, minWidth: size && size.w < 64 ? 96 : undefined }}
          />
        </div>
      );
    }
    if (isMarkdown(file.name) && mode === "read") {
      return (
        <div className="mx-auto max-w-[680px] px-10 pb-16 pt-8">
          <div className="prose-doc">
            <Markdown text={file.content} />
          </div>
        </div>
      );
    }
    const lang = isMarkdown(file.name) ? "markdown" : (CODE_LANG[extOf(file.name)] ?? "");
    // Highlighting very large files makes the pane sluggish; show them plain.
    return file.content.length > 120_000 ? (
      <pre className="code-view selectable min-h-full whitespace-pre p-5 font-mono text-[12.5px]">{file.content}</pre>
    ) : (
      <CodeView content={file.content} lang={lang} />
    );
  }, [file, mode, size]);

  if (!path) {
    return (
      <div className="flex h-full items-center justify-center text-[13px] text-[var(--color-ink-3)]">
        Open a file from {guyName(guy)}'s workspace.
      </div>
    );
  }

  const parts = path.split("/");
  const named = coreFile(path);
  const Icon = named?.icon ?? (file?.encoding === "base64" ? FileImage : isMarkdown(path) ? FileText : FileCode2);
  return (
    <div className="guy-themed flex h-full min-h-0 flex-col" style={{ ["--guy-h" as string]: guyHue(guy.id) }}>
      <div data-tauri-drag-region className="flex h-14 shrink-0 items-center gap-3 border-b border-[var(--color-line)] px-5">
        {/* A peek closes with its ✕; the main pane goes back to what it showed before. */}
        {!onClose && (
          <IconButton label={`Back to ${backTo}`} onClick={leaveFile} className="-ml-2 -mr-1.5">
            <ChevronLeft size={16} />
          </IconButton>
        )}
        <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-[var(--guy-wash-strong)] text-[var(--guy-ink)]">
          <Icon size={15} strokeWidth={2.1} />
        </span>
        <div data-tauri-drag-region className="min-w-0 flex-1">
          <h3 className="flex min-w-0 items-center gap-1.5 font-display text-[14.5px] font-semibold leading-tight">
            <span className="truncate">{named?.label ?? parts[parts.length - 1]}</span>
            {edit && edit.draft !== edit.base && <span title="Not saved yet" className="h-2 w-2 shrink-0 rounded-full bg-[var(--color-accent)]" />}
          </h3>
          <p className="flex min-w-0 gap-3 truncate text-[11.5px] text-[var(--color-ink-3)]">
            <span className="truncate">{named ? named.name : parts.length > 1 ? parts.slice(0, -1).join(" / ") : "Workspace"}</span>
            {file && <span className="shrink-0">{formatBytes(file.size)}</span>}
            {size && <span className="shrink-0">{size.w}×{size.h}</span>}
            {file?.updatedAtMs ? <span className="shrink-0">Edited {shortTime(file.updatedAtMs)}</span> : null}
            {file?.truncated && <span className="shrink-0">Showing the beginning</span>}
          </p>
        </div>
        {edit && (
          <div className="flex items-center gap-1.5">
            <Button size="sm" variant="ghost" onClick={cancelEdit} disabled={edit.saving}>
              Cancel
            </Button>
            <Button size="sm" variant="primary" onClick={() => save(true)} disabled={edit.saving} title={`Save (${shortcut("S")})`}>
              {edit.saving ? <Spinner size={12} /> : null}
              Save
            </Button>
          </div>
        )}
        {!edit && editVia && (
          <IconButton label={startError ?? "Edit"} onClick={startEdit} className={startError ? "text-[var(--color-bad)]" : ""}>
            <Pencil size={13} />
          </IconButton>
        )}
        {!edit && file && isMarkdown(file.name) && (
          <div className="w-36">
            <Segmented
              value={mode}
              onChange={setMode}
              options={[
                { value: "read", label: "Read" },
                { value: "source", label: "Source" },
              ]}
            />
          </div>
        )}
        <div className="flex items-center">
          {absolute && (
            <IconButton
              label={copied ? "Copied" : "Copy path"}
              onClick={() => {
                navigator.clipboard.writeText(absolute);
                setCopied(true);
                setTimeout(() => setCopied(false), 1200);
              }}
            >
              <Copy size={13} />
            </IconButton>
          )}
          {absolute && local && (
            <IconButton label={`Show in ${FILE_MANAGER}`} onClick={() => core.revealPath(absolute).catch(() => {})}>
              <FolderOpen size={13} />
            </IconButton>
          )}
          {!edit && (
            <IconButton label="Reload" onClick={() => setNonce((n) => n + 1)}>
              <RotateCw size={13} />
            </IconButton>
          )}
          {onClose && (
            <IconButton label="Close" onClick={onClose}>
              <X size={14} />
            </IconButton>
          )}
        </div>
      </div>
      {edit && (edit.conflict || edit.error) && (
        <div className="flex shrink-0 items-center gap-2 border-b border-[var(--color-line)] bg-[var(--color-accent-soft)] px-5 py-2 text-[12.5px]">
          <span className="min-w-0 flex-1">
            {edit.conflict ? `${guyName(guy)} changed this file since you started editing it.` : `Couldn't save: ${edit.error}`}
          </span>
          {edit.conflict ? (
            <>
              <Button size="sm" variant="ghost" onClick={takeTheirs}>
                Use theirs
              </Button>
              <Button size="sm" variant="secondary" onClick={() => save(Boolean(edit.conflictExits), edit.conflict!)}>
                Keep mine
              </Button>
            </>
          ) : (
            <Button size="sm" variant="secondary" onClick={() => save(false)}>
              Try again
            </Button>
          )}
        </div>
      )}
      <div className={`min-h-0 flex-1 ${edit ? "overflow-hidden" : "overflow-auto"}`}>
        {edit && path ? (
          <Suspense
            fallback={
              <div className="code-view flex h-full items-center justify-center">
                <Spinner size={18} />
              </div>
            }
          >
            <CodeEditor
              key={draftKey}
              value={edit.draft}
              filename={path.split("/").pop() ?? path}
              onChange={(draft) => {
                const at = drafts.get(draftKey);
                if (at) putEdit(draftKey, { ...at, draft });
              }}
              onSave={() => save(false)}
              onCancel={cancelEdit}
            />
          </Suspense>
        ) : loading && !file ? (
          <div className="flex h-full items-center justify-center">
            <Spinner size={18} />
          </div>
        ) : error ? (
          <div className="mx-auto mt-20 max-w-sm text-center">
            <p className="font-display text-[15px] font-semibold">This file can't be shown here</p>
            <p className="mt-1.5 text-[12.5px] text-[var(--color-ink-3)]">{error}</p>
            {absolute && local && (
              <button
                onClick={() => core.revealPath(absolute).catch(() => {})}
                className="mt-3 text-[12.5px] font-medium text-[var(--guy-ink)] hover:underline"
              >
                Show it in {FILE_MANAGER}
              </button>
            )}
          </div>
        ) : (
          body
        )}
      </div>
    </div>
  );
}
