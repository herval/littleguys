import {
  AlarmClock,
  Archive,
  ArchiveRestore,
  ChevronLeft,
  ChevronRight,
  Folder,
  History,
  MessageCircle,
  MessageSquareCheck,
  MessageSquareDot,
  MoreHorizontal,
  Pencil,
  Pin,
  PinOff,
  Plus,
  Settings as SettingsIcon,
  Sparkles,
  SquarePen,
  Trash2,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { sessions as sessionsApi } from "../lib/gateway";
import {
  activityFor,
  capsOf,
  channelName,
  cleanPreview,
  findGuy,
  gatewayOf,
  guyName,
  guyUid,
  isMultiGateway,
  isRunning,
  RUNTIME_LABEL,
  sessionsFor,
  shortTime,
  hasMainChat,
  threadKind,
  threadTitle,
} from "../lib/guys";
import { core } from "../lib/ipc";
import type { Guy, SessionRow } from "../lib/types";
import { type MainView, mainSessionKey, useApp } from "../stores/app";
import { useArchive } from "../stores/archive";
import { threadId } from "../stores/chat";
import { Avatar } from "../ui/Avatar";
import { IconButton, inputClass, TypingDots } from "../ui/kit";
import { FileTree } from "./Files";
import { GatewayBanner } from "./GatewayBanner";
import { GuyList } from "./GuyList";
import { AbilityList } from "./Skills";

/** Every guy as a card; clicking one drills into their threads (like WhatsApp communities). */
export function Sidebar() {
  const roster = useApp((s) => s.roster);
  const browseUid = useApp((s) => s.browseUid);
  const guy = useMemo(() => findGuy(roster, browseUid), [roster, browseUid]);
  return (
    <aside className="flex w-[300px] shrink-0 flex-col">
      <SidebarTop back={Boolean(guy)} />
      <GatewayBanner />
      {!guy ? (
        <div key="guys" className="animate-push-back flex min-h-0 flex-1 flex-col">
          <h1 className="px-4 pb-2 font-display text-[22px] font-bold tracking-tight">Little guys</h1>
          <GuyList />
        </div>
      ) : (
        <GuyPane key="guy" guy={guy} />
      )}
    </aside>
  );
}

/** Beside the traffic lights: back to the guys, new guy, settings. */
function SidebarTop({ back }: { back: boolean }) {
  const browse = useApp((s) => s.browse);
  const startNewGuy = useApp((s) => s.startNewGuy);
  const setSettingsOpen = useApp((s) => s.setSettingsOpen);
  const canAdd = useApp((s) => s.roster.gateways.some((g) => g.loaded) || s.roster.gateways.length > 1);
  return (
    <div data-tauri-drag-region className="flex h-14 shrink-0 items-center gap-0.5 pl-[84px] pr-2">
      {back && (
        <button
          onClick={() => browse(null)}
          className="-ml-1.5 flex items-center rounded-md py-1 pl-0.5 pr-2 text-[13px] font-medium text-[var(--color-accent-strong)] transition hover:bg-[var(--color-chip)]"
        >
          <ChevronLeft size={17} />
          Little guys
        </button>
      )}
      <div data-tauri-drag-region className="flex-1 self-stretch" />
      {canAdd && (
        <IconButton label="New little guy" onClick={startNewGuy}>
          <Plus size={16} />
        </IconButton>
      )}
      <IconButton label="Settings (⌘,)" onClick={() => setSettingsOpen(true)}>
        <SettingsIcon size={15} />
      </IconButton>
    </div>
  );
}

/** One guy: who they are, then their threads, routines, files or skills. */
function GuyPane({ guy }: { guy: Guy }) {
  const tab = useApp((s) => s.sidebarTab);
  const setTab = useApp((s) => s.setSidebarTab);
  const caps = useApp((s) => capsOf(s.roster, guy.gatewayId));
  const tabs: Array<{ value: MainView; label: string; icon: React.ReactNode }> = [
    { value: "chat", label: "Chats", icon: <MessageCircle size={14} /> },
    ...(caps.routines ? [{ value: "routines" as const, label: "Routines", icon: <AlarmClock size={14} /> }] : []),
    ...(caps.files ? [{ value: "files" as const, label: "Files", icon: <Folder size={14} /> }] : []),
    ...(caps.skills || caps.tools || caps.plugins ? [{ value: "skills" as const, label: "Skills", icon: <Sparkles size={14} /> }] : []),
  ];
  // A tab the guy's runtime doesn't have falls back to its chats.
  const current = tabs.some((t) => t.value === tab) ? tab : "chat";
  return (
    <div className="animate-push flex min-h-0 flex-1 flex-col">
      <GuyHeader guy={guy} />
      {tabs.length > 1 && (
        <div className="flex gap-1 px-3 pb-2.5" role="tablist">
          {tabs.map((t) => {
            const active = current === t.value;
            return (
              <button
                key={t.value}
                role="tab"
                aria-selected={active}
                title={t.label}
                onClick={() => setTab(t.value)}
                className={`flex h-8 items-center justify-center gap-1.5 rounded-full text-[12.5px] font-medium transition-[flex-grow,background-color,color] duration-200 ${
                  active
                    ? "flex-[2.2] bg-[var(--color-ink)] px-3 text-[var(--color-pane-solid)]"
                    : "flex-1 text-[var(--color-ink-2)] hover:bg-[var(--color-chip)] hover:text-[var(--color-ink)]"
                }`}
              >
                {t.icon}
                {active && <span className="truncate">{t.label}</span>}
              </button>
            );
          })}
        </div>
      )}
      {current === "files" ? (
        <FileTree key={guy.uid} guy={guy} />
      ) : current === "skills" ? (
        <AbilityList key={guy.uid} guy={guy} />
      ) : current === "routines" ? (
        <RoutineRunList guy={guy} />
      ) : (
        <ThreadList guy={guy} />
      )}
    </div>
  );
}

function GuyHeader({ guy }: { guy: Guy }) {
  const roster = useApp((s) => s.roster);
  const openEditor = useApp((s) => s.openEditor);
  const act = activityFor(roster, guy);
  const gateway = gatewayOf(roster, guy.gatewayId);
  const caps = capsOf(roster, guy.gatewayId);
  const runtime = gateway?.status.kind && gateway.status.kind !== "openclaw" ? RUNTIME_LABEL[gateway.status.kind] : null;
  const multi = isMultiGateway(roster);
  const model = typeof guy.model === "object" && guy.model ? guy.model.primary : undefined;
  const isAcp = gateway?.status.kind === "acp";
  const subtitle = [
    act.running ? "working…" : isAcp ? guy.gatewayName : model ? model.split("/").pop() : (runtime ?? "ready"),
    multi && !isAcp && guy.gatewayName !== guyName(guy) ? `on ${guy.gatewayName}` : null,
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <div className="px-3 pb-3">
      <div className="flex items-center gap-2.5">
        <Avatar agent={guy} size={36} />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1">
            <h2 className="truncate font-display text-[15px] font-semibold tracking-tight">{guyName(guy)}</h2>
            {caps.manageGuys && (
              <IconButton label="Edit little guy" className="h-6 w-6" onClick={() => openEditor(guy.uid)}>
                <Pencil size={12} />
              </IconButton>
            )}
          </div>
          <p className="truncate text-[11.5px] text-[var(--color-ink-3)]">{subtitle}</p>
        </div>
      </div>
    </div>
  );
}

type Group = { id: string; title: string; rows: SessionRow[]; collapsible: boolean };

function ThreadList({ guy }: { guy: Guy }) {
  const roster = useApp((s) => s.roster);
  const caps = capsOf(roster, guy.gatewayId);
  // The thread open in the chat, if it's this guy's.
  const selected = useApp((s) => (s.selectedUid === guy.uid && s.view === "chat" ? s.selectedSessionKey : null));
  const pastShown = useApp((s) => s.selectedUid === guy.uid && s.view === "past");
  const selectGuy = useApp((s) => s.selectGuy);
  const openPast = useApp((s) => s.openPast);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({ routine: true, helper: true });
  const [creating, setCreating] = useState(false);
  // Archived threads open in place of the list (null: never opened, so nothing slides).
  const archived = useArchive((s) => s.byGuy[guy.uid]);
  const [showArchived, setShowArchived] = useState<boolean | null>(null);

  const withMain = hasMainChat(roster, guy.gatewayId);
  const groups = useMemo<Group[]>(() => {
    const rows = sessionsFor(roster, guy).filter((r) => !r.archived);
    const byActivity = (a: SessionRow, b: SessionRow) =>
      Number(Boolean(b.pinned)) - Number(Boolean(a.pinned)) || (b.updatedAt ?? 0) - (a.updatedAt ?? 0);
    if (!withMain) {
      // ACP agents have no main chat: just their threads, plus the draft while it's open.
      const all = rows.filter((r) => ["main", "chat", "channel"].includes(threadKind(r.key))).sort(byActivity);
      const draftKey = mainSessionKey(guy.id);
      const draft = selected === draftKey && !all.some((r) => r.key === draftKey) ? [{ key: draftKey, kind: "direct", updatedAt: null } as SessionRow] : [];
      return [{ id: "threads", title: "", rows: [...draft, ...all], collapsible: false }];
    }
    const main = rows.find((r) => threadKind(r.key) === "main");
    // Channel conversations sit alongside app chats; ThreadRow badges them with their channel.
    const threads = rows.filter((r) => ["chat", "channel"].includes(threadKind(r.key))).sort(byActivity);
    const mainRow: SessionRow = main ?? ({ key: mainSessionKey(guy.id), kind: "direct", updatedAt: null } as SessionRow);
    return [{ id: "threads", title: "", rows: [mainRow, ...threads], collapsible: false }];
  }, [roster, guy, withMain, selected]);

  const newThread = useCallback(async () => {
    if (creating || !caps.newThreads) return;
    setCreating(true);
    try {
      const res = await sessionsApi.create(guy.gatewayId, guy.id);
      selectGuy(guy.uid, res.key);
      core.refreshRoster(guy.gatewayId).catch(() => {});
    } finally {
      setCreating(false);
    }
  }, [guy.gatewayId, guy.id, guy.uid, creating, selectGuy, caps.newThreads]);

  useEffect(() => {
    const handler = () => newThread();
    document.addEventListener("lg:new-thread", handler);
    return () => document.removeEventListener("lg:new-thread", handler);
  }, [newThread]);

  // Archiving or bringing back a thread, here or anywhere else, changes the guy's threads.
  const threadKeys = useMemo(
    () =>
      sessionsFor(roster, guy)
        .filter((r) => !r.archived)
        .map((r) => r.key)
        .sort()
        .join(" "),
    [roster, guy],
  );
  const { uid, gatewayId, id } = guy;
  useEffect(() => {
    if (caps.threadActions) useArchive.getState().load({ uid, gatewayId, id });
  }, [threadKeys, caps.threadActions, uid, gatewayId, id]);
  // Back to the threads once the last archived one is gone.
  useEffect(() => {
    if (showArchived && archived?.length === 0) setShowArchived(false);
  }, [showArchived, archived?.length]);

  if (showArchived && archived?.length) {
    return <ArchivedList guy={guy} rows={archived} onBack={() => setShowArchived(false)} />;
  }

  return (
    <div className={`flex min-h-0 flex-1 flex-col ${showArchived === false ? "animate-push-back" : ""}`}>
      <div className="flex items-center justify-between pb-1 pl-4 pr-2.5">
        <h3 className="font-display text-[13px] font-semibold">Threads</h3>
        {caps.newThreads && (
          <IconButton label="New thread (⌘N)" onClick={newThread} disabled={creating}>
            <SquarePen size={14} />
          </IconButton>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        {caps.pastSessions && (
          <button
            onClick={() => openPast(guy.uid)}
            title="Sessions from every folder on this Mac, including ones you ran in a terminal"
            className={`mb-1 flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left transition ${
              pastShown ? "bg-[var(--color-accent)] text-white shadow-sm" : "text-[var(--color-ink-2)] hover:bg-[var(--color-chip)]"
            }`}
          >
            <History size={14} className="shrink-0 opacity-80" />
            <span className="font-medium">Resume a session…</span>
          </button>
        )}
        {groups.map((g) => (
          <div key={g.id} className="mb-1">
            {g.title && (
              <button
                onClick={() => setCollapsed((c) => ({ ...c, [g.id]: !c[g.id] }))}
                className="flex w-full items-center gap-1 px-2 pb-1 pt-2.5 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)] hover:text-[var(--color-ink-2)]"
              >
                <ChevronRight size={12} className={`transition ${collapsed[g.id] ? "" : "rotate-90"}`} />
                {g.title}
                <span className="font-normal normal-case tracking-normal opacity-70">{g.rows.length}</span>
              </button>
            )}
            {!(g.collapsible && collapsed[g.id]) &&
              g.rows.map((row) => (
                <ThreadRow
                  key={row.key}
                  mainChat={withMain}
                  actions={caps.threadActions}
                  gatewayId={guy.gatewayId}
                  agentId={guy.id}
                  row={row}
                  running={isRunning(roster, guy.gatewayId, row.key)}
                  selected={selected === row.key}
                  onSelect={() => selectGuy(guy.uid, row.key)}
                />
              ))}
          </div>
        ))}
      </div>
      {archived && archived.length > 0 && (
        <div className="shrink-0 border-t border-[var(--color-line)] px-2 py-1.5">
          <button
            onClick={() => setShowArchived(true)}
            className="flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-[12.5px] text-[var(--color-ink-3)] transition hover:bg-[var(--color-chip)] hover:text-[var(--color-ink-2)]"
          >
            <Archive size={13} className="shrink-0" />
            <span className="font-medium">Archived</span>
            <span className="ml-auto text-[11px] tabular-nums">{archived.length}</span>
          </button>
        </div>
      )}
    </div>
  );
}

/** A guy's archived threads, in place of their threads. Opening one shows it in the chat. */
function ArchivedList({ guy, rows, onBack }: { guy: Guy; rows: SessionRow[]; onBack: () => void }) {
  const roster = useApp((s) => s.roster);
  const caps = capsOf(roster, guy.gatewayId);
  const selected = useApp((s) => (s.selectedUid === guy.uid && s.view === "chat" ? s.selectedSessionKey : null));
  const selectGuy = useApp((s) => s.selectGuy);
  const withMain = hasMainChat(roster, guy.gatewayId);
  return (
    <div className="animate-push flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-1 pb-1 pl-1.5 pr-2.5">
        <IconButton label="Back to threads" onClick={onBack}>
          <ChevronLeft size={16} />
        </IconButton>
        <h3 className="font-display text-[13px] font-semibold">Archived</h3>
        <span className="rounded-full bg-[var(--color-chip)] px-1.5 text-[10.5px] tabular-nums text-[var(--color-ink-3)]">{rows.length}</span>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        {rows.map((row) => (
          <ThreadRow
            key={row.key}
            mainChat={withMain}
            actions={caps.threadActions}
            gatewayId={guy.gatewayId}
            agentId={guy.id}
            row={row}
            running={false}
            selected={selected === row.key}
            onSelect={() => selectGuy(guy.uid, row.key)}
          />
        ))}
      </div>
    </div>
  );
}

/** The Routines tab's sidebar: the routines themselves, then each run's conversation. */
function RoutineRunList({ guy }: { guy: Guy }) {
  const roster = useApp((s) => s.roster);
  const caps = capsOf(roster, guy.gatewayId);
  const run = useApp((s) => s.routineRunByGuy[guy.uid] ?? null);
  // Highlight only what's on screen.
  const shown = useApp((s) => s.selectedUid === guy.uid && s.view === "routines");
  const selectRoutineRun = useApp((s) => s.selectRoutineRun);
  const runs = useMemo(
    () =>
      sessionsFor(roster, guy)
        .filter((r) => !r.archived && threadKind(r.key) === "routine")
        .sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0)),
    [roster, guy],
  );
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center pb-1 pl-4 pr-2.5">
        <h3 className="font-display text-[13px] font-semibold">Routines</h3>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        <button
          onClick={() => selectRoutineRun(null, guy.uid)}
          className={`mb-0.5 flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left transition ${
            shown && !run ? "bg-[var(--color-accent)] text-white shadow-sm" : "hover:bg-[var(--color-chip)]"
          }`}
        >
          <AlarmClock size={14} className="shrink-0 opacity-80" />
          <span className="font-medium">All routines</span>
        </button>
        <p className="flex items-center gap-1.5 px-2 pb-1 pt-3 text-[11.5px] font-medium text-[var(--color-ink-3)]">
          Recent runs
          {runs.length > 0 && <span className="rounded-full bg-[var(--color-chip)] px-1.5 text-[10.5px] tabular-nums">{runs.length}</span>}
        </p>
        {runs.length === 0 ? (
          <p className="px-2 py-1 text-[12px] text-[var(--color-ink-3)]">Each time a routine runs, its conversation shows up here.</p>
        ) : (
          runs.map((row) => (
            <ThreadRow
              key={row.key}
              mainChat
              actions={caps.threadActions}
              gatewayId={guy.gatewayId}
              agentId={guy.id}
              row={row}
              running={isRunning(roster, guy.gatewayId, row.key)}
              selected={shown && run === row.key}
              onSelect={() => selectRoutineRun(row.key, guy.uid)}
            />
          ))
        )}
      </div>
    </div>
  );
}

function ThreadRow({
  mainChat,
  actions,
  gatewayId,
  agentId,
  row,
  running,
  selected,
  onSelect,
}: {
  /** The runtime has a real main chat (not ACP). */
  mainChat: boolean;
  actions: boolean;
  gatewayId: string;
  agentId: string;
  row: SessionRow;
  running: boolean;
  selected: boolean;
  onSelect: () => void;
}) {
  const kind = threadKind(row.key);
  const isMain = kind === "main" && mainChat;
  const isDraft = kind === "main" && !mainChat && !row.updatedAt;
  const [menu, setMenu] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const preview = cleanPreview(row.lastMessagePreview);
  const title = threadTitle(row, mainChat);
  const channel = kind === "channel" ? channelName(row.key) : null;
  // The open thread reads itself, unless you marked it unread from here.
  const kept = useApp((s) => s.keptUnread === threadId(gatewayId, row.key));
  const unread = Boolean(row.unread) && (!selected || kept);
  // The main chat can only be marked read or unread.
  const hasMenu = !isDraft && actions;

  return (
    <div
      role="button"
      tabIndex={0}
      onClick={onSelect}
      onKeyDown={(e) => e.key === "Enter" && onSelect()}
      onContextMenu={(e) => {
        if (!hasMenu) return;
        e.preventDefault();
        setMenu(true);
      }}
      className={`group relative mb-0.5 flex cursor-default items-start gap-2 rounded-lg px-2.5 py-2 outline-none transition ${
        selected ? "bg-[var(--color-accent)] text-white shadow-sm" : "hover:bg-[var(--color-chip)]"
      }`}
    >
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-1.5">
          {row.pinned && !isMain && <Pin size={10} className="shrink-0 self-center opacity-60" />}
          {renaming ? (
            <RenameInput gatewayId={gatewayId} row={row} onDone={() => setRenaming(false)} />
          ) : (
            <span className={`truncate ${unread ? "font-semibold" : "font-medium"}`}>{title}</span>
          )}
          <span className={`ml-auto shrink-0 text-[10.5px] ${selected ? "text-white/75" : "text-[var(--color-ink-3)]"}`}>
            {running ? <TypingDots /> : row.updatedAt ? shortTime(row.updatedAt) : ""}
          </span>
        </div>
        <div className="flex items-center gap-1.5">
          {channel && (
            <span
              className={`shrink-0 rounded px-1 text-[9.5px] font-semibold uppercase tracking-wide ${
                selected ? "bg-white/20" : "bg-[var(--color-chip)] text-[var(--color-ink-3)]"
              }`}
            >
              {channel}
            </span>
          )}
          <p className={`truncate text-[12px] ${selected ? "text-white/80" : "text-[var(--color-ink-3)]"}`}>
            {running ? "working…" : preview || (isMain ? "Your main chat" : isDraft ? "Say hi to start a thread" : "No messages yet")}
          </p>
          {unread && <span className={`ml-auto h-2 w-2 shrink-0 rounded-full ${selected ? "bg-white" : "bg-[var(--color-accent)]"}`} />}
        </div>
      </div>
      {hasMenu && (
        <button
          onClick={(e) => {
            e.stopPropagation();
            setMenu((m) => !m);
          }}
          className={`absolute right-1.5 top-1.5 rounded p-0.5 opacity-0 transition group-hover:opacity-100 ${
            selected ? "hover:bg-white/20" : "hover:bg-black/5 dark:hover:bg-white/10"
          } ${menu ? "opacity-100" : ""}`}
          aria-label="Thread actions"
        >
          <MoreHorizontal size={14} />
        </button>
      )}
      {menu && (
        <ThreadMenu
          isMain={isMain}
          gatewayId={gatewayId}
          agentId={agentId}
          row={row}
          onClose={() => setMenu(false)}
          onRename={() => setRenaming(true)}
        />
      )}
    </div>
  );
}

function RenameInput({ gatewayId, row, onDone }: { gatewayId: string; row: SessionRow; onDone: () => void }) {
  const [value, setValue] = useState(threadTitle(row));
  const commit = async () => {
    const label = value.trim();
    onDone();
    if (label && label !== threadTitle(row)) {
      await sessionsApi.patch(gatewayId, row.key, { label }).catch(() => {});
      core.refreshRoster(gatewayId).catch(() => {});
    }
  };
  return (
    <input
      autoFocus
      value={value}
      onClick={(e) => e.stopPropagation()}
      onChange={(e) => setValue(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Enter") commit();
        if (e.key === "Escape") onDone();
      }}
      className={`${inputClass} h-6 py-0 text-[12.5px] text-[var(--color-ink)]`}
    />
  );
}

function ThreadMenu({
  isMain,
  gatewayId,
  agentId,
  row,
  onClose,
  onRename,
}: {
  isMain: boolean;
  gatewayId: string;
  agentId: string;
  row: SessionRow;
  onClose: () => void;
  onRename: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const selectSession = useApp((s) => s.selectSession);
  const selected = useApp((s) => s.selectedSessionKey);
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [onClose]);

  const act = async (fn: () => Promise<unknown>) => {
    onClose();
    await fn().catch(() => {});
    const refreshed = core.refreshRoster(gatewayId).catch(() => {});
    // Archived threads aren't in the roster: look at the archive again too.
    if (row.archived) refreshed.then(() => useArchive.getState().load({ uid: guyUid(gatewayId, agentId), gatewayId, id: agentId }));
  };
  const leaveIfOpen = () => {
    const s = useApp.getState();
    if (s.selectedUid !== guyUid(gatewayId, agentId)) return;
    if (s.routineRunByGuy[s.selectedUid] === row.key) s.selectRoutineRun(null);
    else if (selected === row.key) selectSession(mainSessionKey(agentId));
  };
  const kind = threadKind(row.key);
  const setUnread = (unread: boolean) =>
    act(async () => {
      const s = useApp.getState();
      // The open thread reads itself: marked unread, it waits until you open it again.
      const open =
        s.selectedUid === guyUid(gatewayId, agentId) &&
        (s.view === "routines" ? s.routineRunByGuy[s.selectedUid] : s.selectedSessionKey) === row.key;
      if (open) s.keepUnread(unread ? threadId(gatewayId, row.key) : null);
      await sessionsApi.patch(gatewayId, row.key, { unread }).catch(() => open && s.keepUnread(null));
    });

  return (
    <div
      ref={ref}
      onClick={(e) => e.stopPropagation()}
      className="animate-pop absolute right-1 top-8 z-20 w-44 rounded-lg border border-[var(--color-line)] bg-[var(--color-pane-solid)] p-1 text-[12.5px] text-[var(--color-ink)] shadow-xl"
    >
      <MenuButton
        icon={row.unread ? <MessageSquareCheck size={13} /> : <MessageSquareDot size={13} />}
        label={row.unread ? "Mark as read" : "Mark as unread"}
        onClick={() => setUnread(!row.unread)}
      />
      {!isMain && (
        <>
          <MenuButton icon={<Pencil size={13} />} label="Rename" onClick={() => (onClose(), onRename())} />
          {row.archived ? (
            <MenuButton
              icon={<ArchiveRestore size={13} />}
              label="Unarchive"
              onClick={() => act(() => sessionsApi.patch(gatewayId, row.key, { archived: false }, row.sessionId ?? undefined))}
            />
          ) : (
            <>
              <MenuButton
                icon={row.pinned ? <PinOff size={13} /> : <Pin size={13} />}
                label={row.pinned ? "Unpin" : "Pin"}
                onClick={() => act(() => sessionsApi.patch(gatewayId, row.key, { pinned: !row.pinned }))}
              />
              <MenuButton
                icon={<Archive size={13} />}
                label="Archive"
                onClick={() =>
                  act(async () => {
                    await sessionsApi.patch(gatewayId, row.key, { archived: true }, row.sessionId ?? undefined);
                    leaveIfOpen();
                  })
                }
              />
            </>
          )}
        </>
      )}
      {(kind === "chat" || kind === "routine" || (kind === "main" && !isMain)) && (
        <MenuButton
          icon={<Trash2 size={13} />}
          label="Delete"
          danger
          onClick={() =>
            act(async () => {
              await sessionsApi.patch(gatewayId, row.key, { archived: true }, row.sessionId ?? undefined);
              await sessionsApi.remove(gatewayId, row.key);
              leaveIfOpen();
            })
          }
        />
      )}
    </div>
  );
}

function MenuButton({ icon, label, onClick, danger }: { icon: React.ReactNode; label: string; onClick: () => void; danger?: boolean }) {
  return (
    <button
      onClick={onClick}
      className={`flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left transition ${
        danger ? "text-[var(--color-bad)] hover:bg-[var(--color-bad)]/10" : "hover:bg-[var(--color-chip)]"
      }`}
    >
      {icon}
      {label}
    </button>
  );
}
