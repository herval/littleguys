// The sidebar grouped by state, workspace or folder: every guy's chats in one list, under
// collapsible headers (grouped by agent, it's the guy cards in GuyList.tsx instead).
import {
  Activity,
  Check,
  ChevronDown,
  ChevronRight,
  FolderGit2,
  FolderPlus,
  Folders,
  MoreHorizontal,
  Pencil,
  Pin,
  Trash2,
  Users,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { refreshCode, useRepoStatus, useRepoStatuses } from "../lib/code";
import { capsOf, channelName, cleanPreview, guyName, hasMainChat, shortTime, splitUid, threadKind, threadTitle } from "../lib/guys";
import {
  type GroupBy,
  groupByFolder,
  groupByState,
  groupByWorkspace,
  type ListedThread,
  listThreads,
  type ThreadGroup,
  workspaceOf,
} from "../lib/threadGroups";
import { doingNow, rowActivity } from "../lib/toolPhrases";
import { useApp } from "../stores/app";
import { threadId } from "../stores/chat";
import { useSidebar } from "../stores/sidebar";
import { Avatar } from "../ui/Avatar";
import { IconButton, inputClass, TypingDots } from "../ui/kit";
import { RepoLine } from "./ChangesPanel";
import { MenuButton, RenameInput, ThreadMenu, useCanFile } from "./ThreadMenu";

/** A group shows this many threads until you ask for the rest. */
const FIRST = 25;
/** What a dragged thread carries (its `gateway|sessionKey`). */
const DRAG_TYPE = "application/x-littleguys-thread";

const OPTIONS: Array<{ value: GroupBy; label: string; icon: React.ReactNode }> = [
  { value: "agent", label: "Agent", icon: <Users size={13} /> },
  { value: "state", label: "State", icon: <Activity size={13} /> },
  { value: "workspace", label: "Workspace", icon: <FolderGit2 size={13} /> },
  { value: "folder", label: "Folder", icon: <Folders size={13} /> },
];

/** How the sidebar is grouped. Workspace needs coder mode: without it, it's by agent. */
export function useGroupBy(): GroupBy {
  const groupBy = useSidebar((s) => s.groupBy);
  const coder = useApp((s) => s.roster.coder?.on ?? false);
  return groupBy === "workspace" && !coder ? "agent" : groupBy;
}

/** Beside the littleguys title: what the list is grouped by (and, by folder, a new folder). */
export function GroupByMenu({ groupBy }: { groupBy: GroupBy }) {
  const setGroupBy = useSidebar((s) => s.setGroupBy);
  const addFolder = useSidebar((s) => s.addFolder);
  const setNaming = useSidebar((s) => s.setNaming);
  const coder = useApp((s) => s.roster.coder?.on ?? false);
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);
  const current = OPTIONS.find((o) => o.value === groupBy) ?? OPTIONS[0];
  const options = OPTIONS.filter((o) => o.value !== "workspace" || coder);
  return (
    <div ref={ref} className="relative flex items-center gap-0.5">
      {groupBy === "folder" && (
        <IconButton label="New folder" onClick={() => setNaming(addFolder("New folder"))}>
          <FolderPlus size={15} />
        </IconButton>
      )}
      <button
        onClick={() => setOpen((o) => !o)}
        title="Group by"
        aria-haspopup="menu"
        aria-expanded={open}
        className={`flex h-7 items-center gap-1 rounded-md pl-2 pr-1.5 text-[12px] font-medium text-[var(--color-ink-2)] transition hover:bg-[var(--color-chip)] hover:text-[var(--color-ink)] ${
          open ? "bg-[var(--color-chip)] text-[var(--color-ink)]" : ""
        }`}
      >
        {current.icon}
        {current.label}
        <ChevronDown size={12} className="opacity-70" />
      </button>
      {open && (
        <div
          role="menu"
          className="animate-pop absolute right-0 top-8 z-20 w-44 rounded-lg border border-[var(--color-line)] bg-[var(--color-pane-solid)] p-1 text-[12.5px] text-[var(--color-ink)] shadow-xl"
        >
          <p className="px-2 pb-1 pt-0.5 text-[11px] font-medium text-[var(--color-ink-3)]">Group by</p>
          {options.map((o) => (
            <button
              key={o.value}
              role="menuitemradio"
              aria-checked={o.value === groupBy}
              onClick={() => {
                setGroupBy(o.value);
                setOpen(false);
              }}
              className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left transition hover:bg-[var(--color-chip)]"
            >
              {o.icon}
              <span className="flex-1">{o.label}</span>
              {o.value === groupBy && <Check size={13} className="text-[var(--color-accent-strong)]" />}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/**
 * Grouped by workspace, a folder whose threads have no repo status yet gets one read (its
 * newest thread's), which stands for all of them. Shown threads read their own as they show.
 */
function useWorkspaceStatuses(threads: ListedThread[] | null) {
  const statuses = useRepoStatuses();
  const known = useRef(statuses);
  known.current = statuses;
  const asked = useRef(new Set<string>());
  useEffect(() => {
    if (!threads) return;
    const answered = new Set(threads.filter((t) => known.current[t.id]).map(workspaceOf));
    for (const t of threads) {
      const dir = workspaceOf(t);
      if (!dir || answered.has(dir) || asked.current.has(dir)) continue;
      asked.current.add(dir);
      void refreshCode(t.guy.gatewayId, t.row.key);
    }
  }, [threads]);
}

export function ThreadGroups({ groupBy }: { groupBy: Exclude<GroupBy, "agent"> }) {
  const roster = useApp((s) => s.roster);
  const folders = useSidebar((s) => s.folders);
  const filed = useSidebar((s) => s.filed);
  const statuses = useRepoStatuses();
  const threads = useMemo(() => listThreads(roster), [roster]);
  useWorkspaceStatuses(groupBy === "workspace" ? threads : null);
  const groups = useMemo(() => {
    if (groupBy === "state") return groupByState(threads);
    if (groupBy === "folder") return groupByFolder(threads, folders, filed);
    return groupByWorkspace(threads, (id) => statuses[id]?.value);
  }, [groupBy, threads, folders, filed, statuses]);
  // The thread open in the chat.
  const selected = useApp((s) =>
    s.view === "chat" && s.selectedUid && s.selectedSessionKey ? threadId(splitUid(s.selectedUid).gatewayId, s.selectedSessionKey) : null,
  );

  return (
    <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
      {groupBy === "folder" && folders.length === 0 && (
        <p className="mx-2 mb-1 mt-1 rounded-lg border border-dashed border-[var(--color-line)] px-3 py-2.5 text-[12px] leading-snug text-[var(--color-ink-3)]">
          Make a folder with <FolderPlus size={12} className="inline align-[-2px]" /> above, then drag threads into it or use a thread's ⋯ menu.
        </p>
      )}
      {groups.map((g) => (
        <Group key={g.id} groupBy={groupBy} group={g} selected={selected} />
      ))}
    </div>
  );
}

function Group({ groupBy, group, selected }: { groupBy: GroupBy; group: ThreadGroup; selected: string | null }) {
  const closedKey = `${groupBy}:${group.id}`;
  const closed = useSidebar((s) => Boolean(s.closed[closedKey]));
  const toggle = useSidebar((s) => s.toggle);
  const file = useSidebar((s) => s.file);
  const [all, setAll] = useState(false);
  const [over, setOver] = useState(false);
  const droppable = groupBy === "folder";
  const shown = all ? group.threads : group.threads.slice(0, FIRST);
  const drop = droppable
    ? {
        onDragOver: (e: React.DragEvent) => {
          if (!e.dataTransfer.types.includes(DRAG_TYPE)) return;
          e.preventDefault();
          e.dataTransfer.dropEffect = "move";
          setOver(true);
        },
        onDragLeave: (e: React.DragEvent) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setOver(false);
        },
        onDrop: (e: React.DragEvent) => {
          e.preventDefault();
          setOver(false);
          const id = e.dataTransfer.getData(DRAG_TYPE);
          if (id) file(id, group.folderId ?? null);
        },
      }
    : {};

  return (
    <div {...drop} className={`mb-1 rounded-xl transition ${over ? "bg-[var(--color-accent)]/10 ring-2 ring-[var(--color-accent)]/40" : ""}`}>
      <GroupHeader group={group} closed={closed} onToggle={() => toggle(closedKey)} />
      {!closed && (
        <>
          {shown.map((t) => (
            <ThreadCard key={t.id} thread={t} selected={selected === t.id} draggable={droppable} />
          ))}
          {group.threads.length > shown.length && (
            <button
              onClick={() => setAll(true)}
              className="w-full rounded-lg px-2.5 py-1.5 text-left text-[12px] font-medium text-[var(--color-accent-strong)] transition hover:bg-[var(--color-chip)]"
            >
              Show {group.threads.length - shown.length} more
            </button>
          )}
          {group.folderId && group.threads.length === 0 && (
            <p className="px-2.5 pb-2 pt-0.5 text-[12px] text-[var(--color-ink-3)]">Drag threads here, or use a thread's ⋯ menu.</p>
          )}
        </>
      )}
    </div>
  );
}

function GroupHeader({ group, closed, onToggle }: { group: ThreadGroup; closed: boolean; onToggle: () => void }) {
  const naming = useSidebar((s) => group.folderId !== undefined && s.naming === group.folderId);
  const [menu, setMenu] = useState(false);
  return (
    <div className="group/header relative flex items-center pr-1">
      {naming && group.folderId ? (
        <FolderName id={group.folderId} name={group.title} />
      ) : (
        <button
          onClick={onToggle}
          onDoubleClick={() => group.folderId && useSidebar.getState().setNaming(group.folderId)}
          title={group.detail}
          className="flex min-w-0 flex-1 items-center gap-1 px-2 pb-1 pt-2.5 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)] hover:text-[var(--color-ink-2)]"
        >
          <ChevronRight size={12} className={`shrink-0 transition ${closed ? "" : "rotate-90"}`} />
          <span className="truncate">{group.title}</span>
          <span className="shrink-0 font-normal normal-case tracking-normal opacity-70">{group.threads.length}</span>
        </button>
      )}
      {group.folderId && !naming && (
        <button
          onClick={() => setMenu((m) => !m)}
          aria-label="Folder actions"
          className={`mt-1.5 rounded p-0.5 text-[var(--color-ink-3)] transition hover:bg-black/5 hover:text-[var(--color-ink)] dark:hover:bg-white/10 ${
            menu ? "opacity-100" : "opacity-0 group-hover/header:opacity-100"
          }`}
        >
          <MoreHorizontal size={14} />
        </button>
      )}
      {menu && group.folderId && <FolderMenu id={group.folderId} onClose={() => setMenu(false)} />}
    </div>
  );
}

/** Typing a folder's name, in its header. */
function FolderName({ id, name }: { id: string; name: string }) {
  const renameFolder = useSidebar((s) => s.renameFolder);
  const setNaming = useSidebar((s) => s.setNaming);
  const [value, setValue] = useState(name);
  const commit = () => {
    const next = value.trim();
    if (next && next !== name) renameFolder(id, next);
    setNaming(null);
  };
  return (
    <div className="flex-1 px-1 pb-1 pt-1.5">
      <input
        autoFocus
        value={value}
        onFocus={(e) => e.currentTarget.select()}
        onChange={(e) => setValue(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
          if (e.key === "Escape") setNaming(null);
        }}
        aria-label="Folder name"
        className={`${inputClass} h-7 py-0 text-[12.5px]`}
      />
    </div>
  );
}

function FolderMenu({ id, onClose }: { id: string; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  const setNaming = useSidebar((s) => s.setNaming);
  const removeFolder = useSidebar((s) => s.removeFolder);
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [onClose]);
  return (
    <div
      ref={ref}
      className="animate-pop absolute right-1 top-8 z-20 w-48 rounded-lg border border-[var(--color-line)] bg-[var(--color-pane-solid)] p-1 text-[12.5px] text-[var(--color-ink)] shadow-xl"
    >
      <MenuButton icon={<Pencil size={13} />} label="Rename" onClick={() => (onClose(), setNaming(id))} />
      {/* Its threads aren't touched: they go back to everything else. */}
      <MenuButton icon={<Trash2 size={13} />} label="Delete folder" danger onClick={() => (onClose(), removeFolder(id))} />
    </div>
  );
}

/** One thread, with its guy's face (click it for all of that guy's threads). */
function ThreadCard({ thread, selected, draggable }: { thread: ListedThread; selected: boolean; draggable: boolean }) {
  const { guy, row, state } = thread;
  const roster = useApp((s) => s.roster);
  const openThread = useApp((s) => s.openThread);
  const browse = useApp((s) => s.browse);
  const mainChat = hasMainChat(roster, guy.gatewayId);
  const caps = capsOf(roster, guy.gatewayId);
  const kind = threadKind(row.key);
  // A guy's main chat is just them, like a DM.
  const isMain = mainChat && kind === "main";
  const running = state === "working";
  const needsYou = state === "needs-you";
  // The open thread reads itself, unless you marked it unread.
  const kept = useApp((s) => s.keptUnread === thread.id);
  const unread = Boolean(row.unread) && (!selected || kept);
  const repo = useRepoStatus(guy.gatewayId, row.key, running);
  const canFile = useCanFile();
  const [menu, setMenu] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const hasMenu = caps.threadActions || canFile;
  const channel = kind === "channel" ? channelName(row.key) : null;
  const preview = cleanPreview(row.lastMessagePreview);
  const ring = needsYou ? "needs-you" : running ? "working" : unread ? "unread" : "idle";
  const open = () => openThread(guy.uid, row.key);

  return (
    <div
      role="button"
      tabIndex={0}
      draggable={draggable && !renaming}
      onDragStart={(e) => {
        e.dataTransfer.setData(DRAG_TYPE, thread.id);
        e.dataTransfer.effectAllowed = "move";
      }}
      onClick={open}
      onKeyDown={(e) => e.key === "Enter" && e.currentTarget === e.target && open()}
      onContextMenu={(e) => {
        if (!hasMenu) return;
        e.preventDefault();
        setMenu(true);
      }}
      className={`group relative mb-0.5 flex cursor-default items-center gap-2.5 rounded-xl px-2 py-2 outline-none transition ${
        selected ? "bg-[var(--color-chip)]" : "hover:bg-[var(--color-chip)]"
      }`}
    >
      <button
        onClick={(e) => {
          e.stopPropagation();
          browse(guy.uid);
        }}
        title={`All of ${guyName(guy)}'s threads`}
        className="shrink-0 rounded-full"
      >
        <Avatar agent={guy} size={36} ring={ring} />
      </button>
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-1.5">
          {row.pinned && !isMain && <Pin size={10} className="shrink-0 self-center opacity-60" />}
          {renaming ? (
            <RenameInput gatewayId={guy.gatewayId} row={row} onDone={() => setRenaming(false)} />
          ) : (
            <span className={`truncate text-[13px] ${unread ? "font-semibold" : "font-medium"}`}>{isMain ? guyName(guy) : threadTitle(row, mainChat)}</span>
          )}
          {!isMain && !renaming && <span className="max-w-[38%] shrink-0 truncate text-[11px] text-[var(--color-ink-3)]">{guyName(guy)}</span>}
          <span className={`ml-auto shrink-0 text-[10.5px] ${unread ? "font-medium text-[var(--color-accent-strong)]" : "text-[var(--color-ink-3)]"}`}>
            {running ? <TypingDots /> : row.updatedAt ? shortTime(row.updatedAt) : ""}
          </span>
        </div>
        <div className="mt-0.5 flex items-center gap-1.5">
          {channel && (
            <span className="shrink-0 rounded bg-[var(--color-chip)] px-1 text-[9.5px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">
              {channel}
            </span>
          )}
          {needsYou ? (
            <p className="truncate text-[12px] font-medium text-[var(--color-bad)]">needs your OK</p>
          ) : repo && !running ? (
            <RepoLine gatewayId={guy.gatewayId} sessionKey={row.key} status={repo} selected={false} />
          ) : (
            <p className={`min-w-0 flex-1 truncate text-[12px] ${unread ? "text-[var(--color-ink)]" : "text-[var(--color-ink-3)]"}`}>
              {running ? (doingNow(rowActivity(row)) ?? "working…") : preview || (isMain ? "Say hi" : "No messages yet")}
            </p>
          )}
          {unread && <span className="ml-auto h-2 w-2 shrink-0 rounded-full bg-[var(--color-accent)]" />}
        </div>
      </div>
      {hasMenu && (
        <button
          onClick={(e) => {
            e.stopPropagation();
            setMenu((m) => !m);
          }}
          className={`absolute right-1.5 top-1.5 rounded bg-[var(--color-chip)] p-0.5 opacity-0 transition hover:bg-black/5 group-hover:opacity-100 dark:hover:bg-white/10 ${
            menu ? "opacity-100" : ""
          }`}
          aria-label="Thread actions"
        >
          <MoreHorizontal size={14} />
        </button>
      )}
      {menu && (
        <ThreadMenu
          isMain={isMain}
          actions={caps.threadActions}
          gatewayId={guy.gatewayId}
          agentId={guy.id}
          row={row}
          onClose={() => setMenu(false)}
          onRename={() => setRenaming(true)}
        />
      )}
    </div>
  );
}
