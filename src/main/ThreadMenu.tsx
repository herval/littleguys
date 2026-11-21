import {
  Archive,
  ArchiveRestore,
  Check,
  ChevronLeft,
  FolderInput,
  FolderMinus,
  FolderPlus,
  MessageSquareCheck,
  MessageSquareDot,
  Pencil,
  Pin,
  PinOff,
  SquareTerminal,
  Trash2,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { openTerminal, useTerminalOn } from "../lib/code";
import { sessions as sessionsApi } from "../lib/gateway";
import { guyUid, threadKind, threadTitle } from "../lib/guys";
import { core } from "../lib/ipc";
import type { SessionRow } from "../lib/types";
import { mainSessionKey, useApp } from "../stores/app";
import { useArchive } from "../stores/archive";
import { threadId } from "../stores/chat";
import { useSidebar } from "../stores/sidebar";
import { inputClass } from "../ui/kit";
import { reportError } from "../ui/reportError";

/** Renaming a thread in place, in its row. */
export function RenameInput({ gatewayId, row, onDone }: { gatewayId: string; row: SessionRow; onDone: () => void }) {
  const [value, setValue] = useState(threadTitle(row));
  const commit = async () => {
    const label = value.trim();
    onDone();
    if (label && label !== threadTitle(row)) {
      await sessionsApi.patch(gatewayId, row.key, { label }).catch(reportError);
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

/** Whether threads can go in folders: once you've made one, or while the sidebar groups by folder. */
export function useCanFile(): boolean {
  return useSidebar((s) => s.folders.length > 0 || s.groupBy === "folder");
}

/**
 * A thread's ⋯ menu. `actions`: the runtime lets you mark, rename, pin, archive and delete
 * threads (Move to folder is littleguys' own, so it's there either way).
 */
export function ThreadMenu({
  isMain,
  actions = true,
  gatewayId,
  agentId,
  row,
  onClose,
  onRename,
}: {
  isMain: boolean;
  actions?: boolean;
  gatewayId: string;
  agentId: string;
  row: SessionRow;
  onClose: () => void;
  onRename: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const selectSession = useApp((s) => s.selectSession);
  const selected = useApp((s) => s.selectedSessionKey);
  const canFile = useCanFile();
  const [filing, setFiling] = useState(false);
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [onClose]);

  const act = async (fn: () => Promise<unknown>) => {
    onClose();
    try { await fn(); } catch (error) { await reportError(error); return; }
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
  const terminalOn = useTerminalOn(gatewayId);
  const openTerminalHere = async () => {
    onClose();
    try {
      const note = await openTerminal(gatewayId, row.key);
      if (note) await reportError(note, "Opened another terminal", "info");
    } catch (error) {
      await reportError(error, "Couldn't open a terminal");
    }
  };
  const setUnread = (unread: boolean) =>
    act(async () => {
      const s = useApp.getState();
      // The open thread reads itself: marked unread, it waits until you open it again.
      const open =
        s.selectedUid === guyUid(gatewayId, agentId) &&
        (s.view === "routines" ? s.routineRunByGuy[s.selectedUid] : s.selectedSessionKey) === row.key;
      if (open) s.keepUnread(unread ? threadId(gatewayId, row.key) : null);
      await sessionsApi.patch(gatewayId, row.key, { unread }).catch((error) => { if (open) s.keepUnread(null); throw error; });
    });

  return (
    <div
      ref={ref}
      onClick={(e) => e.stopPropagation()}
      className="animate-pop absolute right-1 top-8 z-20 w-48 rounded-lg border border-[var(--color-line)] bg-[var(--color-pane-solid)] p-1 text-[12.5px] text-[var(--color-ink)] shadow-xl"
    >
      {filing ? (
        <FolderChoices id={threadId(gatewayId, row.key)} onBack={() => setFiling(false)} onDone={onClose} />
      ) : (
        <>
          {actions && (
            <MenuButton
              icon={row.unread ? <MessageSquareCheck size={13} /> : <MessageSquareDot size={13} />}
              label={row.unread ? "Mark as read" : "Mark as unread"}
              onClick={() => setUnread(!row.unread)}
            />
          )}
          {terminalOn && <MenuButton icon={<SquareTerminal size={13} />} label="Open a terminal here" onClick={openTerminalHere} />}
          {canFile && !row.archived && <MenuButton icon={<FolderInput size={13} />} label="Move to folder…" onClick={() => setFiling(true)} />}
          {actions && !isMain && (
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
          {actions && (kind === "chat" || kind === "routine" || (kind === "main" && !isMain)) && (
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
        </>
      )}
    </div>
  );
}

/** Move to folder: your folders (a check on the one it's in), a new one, or out of it. */
function FolderChoices({ id, onBack, onDone }: { id: string; onBack: () => void; onDone: () => void }) {
  const folders = useSidebar((s) => s.folders);
  const current = useSidebar((s) => s.filed[id]);
  const file = useSidebar((s) => s.file);
  const addFolder = useSidebar((s) => s.addFolder);
  const [naming, setNaming] = useState(folders.length === 0);
  const [name, setName] = useState("");
  const into = (folderId: string | null) => {
    file(id, folderId);
    onDone();
  };
  const create = () => {
    const n = name.trim();
    if (n) into(addFolder(n));
  };
  return (
    <>
      <button
        onClick={onBack}
        className="flex w-full items-center gap-1 rounded-md px-1.5 py-1 text-left text-[11.5px] font-medium text-[var(--color-ink-3)] transition hover:bg-[var(--color-chip)]"
      >
        <ChevronLeft size={13} />
        Move to folder
      </button>
      <div className="max-h-56 overflow-y-auto">
        {folders.map((f) => (
          <MenuButton
            key={f.id}
            icon={<Check size={13} className={f.id === current ? "" : "invisible"} />}
            label={f.name}
            onClick={() => into(f.id === current ? null : f.id)}
          />
        ))}
      </div>
      {naming ? (
        <input
          autoFocus
          value={name}
          placeholder="New folder"
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => {
            e.stopPropagation();
            if (e.key === "Enter") create();
            if (e.key === "Escape") (folders.length ? setNaming(false) : onBack());
          }}
          className={`${inputClass} mt-0.5 h-7 py-0 text-[12.5px]`}
        />
      ) : (
        <MenuButton icon={<FolderPlus size={13} />} label="New folder…" onClick={() => setNaming(true)} />
      )}
      {current && folders.some((f) => f.id === current) && (
        <MenuButton icon={<FolderMinus size={13} />} label="Take out of folder" onClick={() => into(null)} />
      )}
    </>
  );
}

export function MenuButton({ icon, label, onClick, danger }: { icon: React.ReactNode; label: string; onClick: () => void; danger?: boolean }) {
  return (
    <button
      onClick={onClick}
      className={`flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left transition ${
        danger ? "text-[var(--color-bad)] hover:bg-[var(--color-bad)]/10" : "hover:bg-[var(--color-chip)]"
      }`}
    >
      {icon}
      <span className="truncate">{label}</span>
    </button>
  );
}
