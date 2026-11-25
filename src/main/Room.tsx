// A room on screen: its header (who's in it, each one's part), who's working, and the
// dialog that makes one. The chat itself is ChatView's, like any thread's.
import { ArrowUpRight, Check, ChevronDown, Folder, MoreHorizontal, Pencil, Plus, Trash2, UserMinus, Users } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { allGuys, followsSessions, gatewayOf, guyName, isMultiGateway, isOnThisMac, isRoom } from "../lib/guys";
import { core, errorMessage, isTauri } from "../lib/ipc";
import { rooms as roomsApi, type Seat, seatFace, seatsOf, useRooms, workingLine } from "../lib/rooms";
import type { Attention, Guy, RoomInfo } from "../lib/types";
import { mainSessionKey, useApp } from "../stores/app";
import { Avatar } from "../ui/Avatar";
import { confirmAction } from "../ui/confirm";
import { Button, Field, IconButton, inputClass, Modal, TypingDots } from "../ui/kit";
import { reportError } from "../ui/reportError";
import { tildify } from "./FolderPicker";

/** A room's members, each with the guy it is while its bot is connected. */
export function useSeats(room: RoomInfo | undefined): Seat[] {
  const roster = useApp((s) => s.roster);
  return useMemo(() => (room ? seatsOf(roster, room) : []), [roster, room]);
}

/** Who's working in a room: asked once when it opens, then kept up by `room.state` events. */
export function useRoomState(id: string) {
  const state = useRooms((s) => s.states[id]);
  useEffect(() => {
    roomsApi.state(id).then((s) => useRooms.getState().set(s)).catch(() => {});
  }, [id]);
  return state;
}

/** Closes a popup on a click outside it or Escape. */
function useDismiss(ref: React.RefObject<HTMLElement | null>, onClose: () => void) {
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [ref, onClose]);
}

const menuClass =
  "animate-pop absolute z-30 rounded-xl border border-[var(--color-line)] bg-[var(--color-pane-solid)] p-1 text-[12.5px] text-[var(--color-ink)] shadow-xl";
const itemClass = "flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left transition hover:bg-[var(--color-chip)] disabled:opacity-45";

/** The room's name, who leads and who listens, and a chip per member that opens its menu. */
export function RoomHeader({ guy, room }: { guy: Guy; room: RoomInfo }) {
  const seats = useSeats(room);
  const state = useRoomState(guy.id);
  const [open, setOpen] = useState<string | null>(null);
  const [renaming, setRenaming] = useState(false);
  const working = workingLine(state, seats);
  const lead = seats.find((s) => s.lead);
  const listening = seats.filter((s) => s.attention === "listening").map((s) => s.name);
  const subtitle = [
    lead ? `${lead.name} leads` : null,
    listening.length ? `${listening.join(", ")} ${listening.length === 1 ? "is" : "are"} listening` : null,
  ].filter(Boolean);

  return (
    <div data-tauri-drag-region className="flex h-14 shrink-0 items-center gap-3 border-b border-[var(--color-line)] px-5">
      <Avatar agent={guy} size={34} ring={working ? "working" : "none"} />
      <div data-tauri-drag-region className="min-w-0 flex-1">
        {renaming ? (
          <RenameRoom guy={guy} onDone={() => setRenaming(false)} />
        ) : (
          <h3 className="truncate font-display text-[14.5px] font-semibold leading-tight">{guyName(guy)}</h3>
        )}
        <p className="mt-0.5 truncate text-[11.5px] leading-tight">
          {working ? (
            <span className="text-[var(--color-accent-strong)]">{working}…</span>
          ) : (
            <span className="text-[var(--color-ink-3)]">{subtitle.join(" · ") || `${seats.length} guys`}</span>
          )}
        </p>
      </div>
      <div className="flex shrink-0 items-center gap-1.5">
        {seats.map((seat) => (
          <div key={seat.uid} className="relative">
            <button
              data-member={seat.uid}
              aria-haspopup="menu"
              aria-expanded={open === seat.uid}
              onClick={() => setOpen(open === seat.uid ? null : seat.uid)}
              title={seat.guy ? `${seat.name} on ${seat.guy.gatewayName}` : `${seat.name} is away`}
              className={`flex h-7 items-center gap-1.5 rounded-full border py-0.5 pl-0.5 pr-2.5 text-[12px] transition ${
                open === seat.uid ? "border-transparent bg-[var(--color-chip)]" : "border-[var(--color-line)] hover:bg-[var(--color-chip)]"
              } ${seat.guy ? "" : "opacity-55"}`}
            >
              <Avatar agent={seatFace(seat)} size={22} />
              <span className="max-w-[90px] truncate text-[var(--color-ink-2)]">{seat.name}</span>
              {(seat.lead || seat.attention === "listening") && (
                <span className="text-[11px] text-[var(--color-ink-3)]">{[seat.lead && "lead", seat.attention === "listening" && "listening"].filter(Boolean).join(", ")}</span>
              )}
            </button>
            {open === seat.uid && <MemberMenu guy={guy} seat={seat} canRemove={seats.length > 2} onClose={() => setOpen(null)} />}
          </div>
        ))}
        <div className="relative">
          <IconButton label="Add a guy to this room" onClick={() => setOpen(open === "+" ? null : "+")} className="rounded-full border border-dashed border-[var(--color-ink-3)]/60">
            <Plus size={13} />
          </IconButton>
          {open === "+" && <AddMember guy={guy} room={room} onClose={() => setOpen(null)} />}
        </div>
        <div className="relative">
          <IconButton label="Room options" onClick={() => setOpen(open === "…" ? null : "…")}>
            <MoreHorizontal size={15} />
          </IconButton>
          {open === "…" && <RoomMenu guy={guy} onRename={() => setRenaming(true)} onClose={() => setOpen(null)} />}
        </div>
      </div>
    </div>
  );
}

function RenameRoom({ guy, onDone }: { guy: Guy; onDone: () => void }) {
  const [value, setValue] = useState(guyName(guy));
  const commit = () => {
    const name = value.trim();
    onDone();
    if (name && name !== guyName(guy)) roomsApi.update(guy.id, { name }).catch(reportError);
  };
  return (
    <input
      autoFocus
      value={value}
      onChange={(e) => setValue(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") commit();
        if (e.key === "Escape") onDone();
      }}
      className={`${inputClass} h-6 max-w-[240px] py-0 text-[13px]`}
    />
  );
}

const ATTENTION: Array<{ value: Attention; label: string; detail: string }> = [
  { value: "oncall", label: "On call", detail: "Speaks only when someone addresses it" },
  { value: "listening", label: "Listening", detail: "Also gets one look when the room goes quiet" },
];

/** One member's part in the room: when it speaks, whether it leads, its own side of things. */
function MemberMenu({ guy, seat, canRemove, onClose }: { guy: Guy; seat: Seat; canRemove: boolean; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useDismiss(ref, onClose);
  const act = (change: Parameters<typeof roomsApi.update>[1]) => {
    onClose();
    roomsApi.update(guy.id, change).catch(reportError);
  };
  // Guys whose threads each work in a folder on this computer: the room can say which.
  const worksInFolders = useApp((s) => {
    const g = gatewayOf(s.roster, seat.guy?.gatewayId);
    return Boolean(g) && ((g?.status.kind === "acp" && isOnThisMac(g)) || followsSessions(g?.status.kind));
  });
  const pickFolder = async () => {
    onClose();
    if (!isTauri) return;
    const { open } = await import("@tauri-apps/plugin-dialog");
    const dir = await open({ directory: true, multiple: false, defaultPath: seat.folder ?? undefined, title: `Where should ${seat.name} work for this room?` });
    if (typeof dir === "string") roomsApi.update(guy.id, { folder: { [seat.uid]: dir } }).catch(reportError);
  };
  return (
    <div ref={ref} className={`${menuClass} right-0 top-9 w-64`}>
      <div className="flex items-center gap-2.5 px-2.5 py-2">
        <Avatar agent={seatFace(seat)} size={28} />
        <div className="min-w-0">
          <p className="truncate font-medium">{seat.name}</p>
          <p className="truncate text-[11.5px] text-[var(--color-ink-3)]">{seat.guy ? seat.guy.gatewayName : "Away: its bot isn't connected"}</p>
        </div>
      </div>
      <div className="mx-1 my-1 border-t border-[var(--color-line)]" />
      <p className="px-2.5 pb-0.5 pt-1 text-[10.5px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">In this room</p>
      {ATTENTION.map((a) => (
        <button key={a.value} role="menuitemradio" aria-checked={seat.attention === a.value} className={itemClass} onClick={() => act({ attention: { [seat.uid]: a.value } })}>
          <span
            className={`h-3.5 w-3.5 shrink-0 rounded-full border-[1.5px] ${
              seat.attention === a.value ? "border-[4.5px] border-[var(--color-accent)]" : "border-[var(--color-ink-3)]"
            }`}
          />
          <span className="min-w-0">
            <span className="block">{a.label}</span>
            <span className="block text-[11.5px] text-[var(--color-ink-3)]">{a.detail}</span>
          </span>
        </button>
      ))}
      <div className="mx-1 my-1 border-t border-[var(--color-line)]" />
      {seat.lead ? (
        <p className="flex items-center gap-2 px-2.5 py-1.5 text-[var(--color-ink-3)]">
          <Check size={13} /> Leads the room
        </p>
      ) : (
        <button className={itemClass} onClick={() => act({ lead: seat.uid })}>
          Make {seat.name} the lead
        </button>
      )}
      {worksInFolders && (
        <button className={itemClass} onClick={pickFolder} title={seat.seat ? "It starts a new thread there, with the last of the room for context" : undefined}>
          <Folder size={13} className="shrink-0" />
          <span className="min-w-0 truncate">{seat.folder ? `Works in ${tildify(seat.folder)}` : "Choose its folder…"}</span>
        </button>
      )}
      {seat.seat && seat.guy && (
        <button
          className={itemClass}
          onClick={() => {
            onClose();
            useApp.getState().openThread(seat.uid, seat.seat as string);
          }}
        >
          <ArrowUpRight size={13} /> Open {seat.name}'s side
        </button>
      )}
      <button className={`${itemClass} text-[var(--color-bad)]`} disabled={!canRemove} title={canRemove ? undefined : "A room needs at least two guys"} onClick={() => act({ remove: [seat.uid] })}>
        <UserMinus size={13} /> Remove from {guyName(guy)}
      </button>
    </div>
  );
}

function AddMember({ guy, room, onClose }: { guy: Guy; room: RoomInfo; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useDismiss(ref, onClose);
  const roster = useApp((s) => s.roster);
  const others = useMemo(() => allGuys(roster).filter((g) => !isRoom(g) && !room.members.some((m) => m.uid === g.uid)), [roster, room]);
  const multi = isMultiGateway(roster);
  return (
    <div ref={ref} className={`${menuClass} right-0 top-9 max-h-[320px] w-60 overflow-y-auto`}>
      {others.length === 0 && <p className="px-2.5 py-2 text-[var(--color-ink-3)]">Everyone's already here.</p>}
      {others.map((g) => (
        <button
          key={g.uid}
          className={itemClass}
          onClick={() => {
            onClose();
            roomsApi.update(guy.id, { add: [g.uid] }).catch(reportError);
          }}
        >
          <Avatar agent={g} size={24} />
          <span className="min-w-0 flex-1 truncate">{guyName(g)}</span>
          {multi && <span className="shrink-0 text-[11px] text-[var(--color-ink-3)]">{g.gatewayName}</span>}
        </button>
      ))}
    </div>
  );
}

function RoomMenu({ guy, onRename, onClose }: { guy: Guy; onRename: () => void; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useDismiss(ref, onClose);
  return (
    <div ref={ref} className={`${menuClass} right-0 top-9 w-44`}>
      <button
        className={itemClass}
        onClick={() => {
          onClose();
          onRename();
        }}
      >
        <Pencil size={13} /> Rename
      </button>
      <button
        className={`${itemClass} text-[var(--color-bad)]`}
        onClick={async () => {
          onClose();
          const ok = await confirmAction(`Delete ${guyName(guy)}? Its conversation goes with it. Each guy keeps its own side as a thread.`, "Delete room");
          if (ok) roomsApi.remove(guy.id).catch(reportError);
        }}
      >
        <Trash2 size={13} /> Delete room
      </button>
    </div>
  );
}

/** Under the transcript: who's working right now, or the guys waiting for you to let them go on. */
export function RoomStatus({ guy, room }: { guy: Guy; room: RoomInfo }) {
  const seats = useSeats(room);
  const state = useRooms((s) => s.states[guy.id]);
  const [busy, setBusy] = useState(false);
  const line = workingLine(state, seats);
  if (line) {
    return (
      <p className="mt-3 flex items-center gap-2 pl-9 text-[12px] text-[var(--color-ink-3)]">
        <TypingDots /> {line}
      </p>
    );
  }
  if (!(state?.paused ?? room.paused)) return null;
  return (
    <div className="mx-auto mt-4 max-w-md text-center text-[12.5px] text-[var(--color-ink-2)]">
      <p>They've been going back and forth for a while, so they stopped to check with you.</p>
      <Button
        className="mt-2.5"
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          await roomsApi.resume(guy.id).catch(reportError);
          setBusy(false);
        }}
      >
        Let them continue
      </Button>
    </div>
  );
}

/** In the composer: who the message is for, and a menu to send it to someone else. */
export function ToChip({ seats, to, named, onPick }: { seats: Seat[]; to: string[]; /** The text names them itself. */ named: boolean; onPick: (to: string[]) => void }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useDismiss(ref, () => setOpen(false));
  const everyone = to.length === seats.length && seats.length > 1;
  const first = seats.find((s) => s.uid === to[0]);
  const label = everyone ? "everyone" : to.map((uid) => seats.find((s) => s.uid === uid)?.name).filter(Boolean).join(", ");
  return (
    <div ref={ref} className="relative mb-[calc(18px_-_0.8125rem)] shrink-0">
      <button
        onClick={() => setOpen(!open)}
        disabled={named}
        title={named ? "The message names who it's for" : "Who this message is for"}
        className="flex h-[26px] max-w-[220px] items-center gap-1 rounded-full bg-[var(--color-accent-soft)] py-0.5 pl-0.5 pr-2 text-[12px] font-medium text-[var(--color-accent-strong)] transition enabled:hover:brightness-95"
      >
        {everyone || !first ? <Users size={13} className="ml-1.5" /> : <Avatar agent={seatFace(first)} size={22} />}
        <span className="truncate">to {label || "the room"}</span>
        {!named && <ChevronDown size={12} className="shrink-0" />}
      </button>
      {open && (
        <div className={`${menuClass} bottom-9 left-0 w-56`}>
          {seats.map((seat) => (
            <button
              key={seat.uid}
              className={itemClass}
              onClick={() => {
                setOpen(false);
                onPick([seat.uid]);
              }}
            >
              <Avatar agent={seatFace(seat)} size={22} />
              <span className="min-w-0 flex-1 truncate">{seat.name}</span>
              {!everyone && to.length === 1 && to[0] === seat.uid && <Check size={13} className="text-[var(--color-accent)]" />}
            </button>
          ))}
          <div className="mx-1 my-1 border-t border-[var(--color-line)]" />
          <button
            className={itemClass}
            onClick={() => {
              setOpen(false);
              onPick(seats.map((s) => s.uid));
            }}
          >
            <span className="flex h-[22px] w-[22px] items-center justify-center rounded-full bg-[var(--color-chip)]">
              <Users size={12} />
            </span>
            <span className="flex-1">Everyone</span>
            {everyone && <Check size={13} className="text-[var(--color-accent)]" />}
          </button>
        </div>
      )}
    </div>
  );
}

export interface MentionOption {
  /** What goes after the `@`. */
  handle: string;
  label: string;
  detail: string;
  seat?: Seat;
}

/** What `@` offers: the members whose name or handle starts with what's typed, then everyone. */
export function mentionOptions(seats: Seat[], query: string): MentionOption[] {
  const q = query.toLowerCase();
  const fits = (s: string) => s.toLowerCase().startsWith(q);
  const members = seats
    .filter((s) => fits(s.handle) || fits(s.name))
    .map((s) => ({
      handle: s.handle,
      label: s.name,
      detail: [s.guy?.gatewayName ?? "away", s.lead ? "lead" : null, s.attention === "listening" ? "listening" : "on call"].filter(Boolean).join(" · "),
      seat: s,
    }));
  return fits("everyone") ? [...members, { handle: "everyone", label: "everyone", detail: `all ${seats.length} get a turn` }] : members;
}

/** The popup above the composer while an `@` is being typed. */
export function MentionMenu({ options, active, onHover, onPick }: { options: MentionOption[]; active: number; onHover: (i: number) => void; onPick: (i: number) => void }) {
  return (
    <div className="absolute bottom-full left-0 z-20 mb-2 w-72 overflow-hidden rounded-xl border border-[var(--color-line)] bg-[var(--color-pane-solid)] p-1 shadow-xl" role="listbox" id="mention-menu">
      {options.map((o, i) => (
        <div
          key={o.handle}
          id={`mention-${i}`}
          role="option"
          aria-selected={i === active}
          onMouseMove={() => i !== active && onHover(i)}
          onMouseDown={(e) => {
            // Keep focus in the textarea.
            e.preventDefault();
            onPick(i);
          }}
          className={`flex items-center gap-2.5 rounded-lg px-2.5 py-1.5 ${i === active ? "bg-[var(--color-chip)]" : ""}`}
        >
          {o.seat ? (
            <Avatar agent={seatFace(o.seat)} size={26} />
          ) : (
            <span className="flex h-[26px] w-[26px] items-center justify-center rounded-full bg-[var(--color-chip)]">
              <Users size={13} />
            </span>
          )}
          <span className="min-w-0">
            <span className="block truncate text-[13px]">{o.label}</span>
            <span className="block truncate text-[11.5px] text-[var(--color-ink-3)]">{o.detail}</span>
          </span>
        </div>
      ))}
    </div>
  );
}

/** "New room": pick the guys, name it, say who leads. */
export function NewRoom({ onClose }: { onClose: () => void }) {
  const roster = useApp((s) => s.roster);
  const guys = useMemo(() => allGuys(roster).filter((g) => !isRoom(g)), [roster]);
  const multi = isMultiGateway(roster);
  const [picked, setPicked] = useState<string[]>([]);
  const [name, setName] = useState("");
  const [lead, setLead] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const leadUid = lead && picked.includes(lead) ? lead : (picked[0] ?? null);
  const toggle = (uid: string) => setPicked((p) => (p.includes(uid) ? p.filter((u) => u !== uid) : [...p, uid]));

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const made = await roomsApi.create({ name: name.trim() || undefined, members: picked, lead: leadUid ?? undefined });
      await core.refreshRoster("rooms").catch(() => {});
      onClose();
      useApp.getState().openThread(`rooms/${made.id}`, mainSessionKey(made.id));
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  return (
    <Modal onClose={onClose} width={460}>
      <div className="p-6">
        <h2 className="font-display text-[17px] font-semibold">New room</h2>
        <p className="mt-1 text-[12.5px] text-[var(--color-ink-3)]">
          A chat with several of your guys in it. One answers you unless you name another with @, and they can bring each other in.
        </p>
        {guys.length < 2 ? (
          <p className="mt-5 text-[13px] text-[var(--color-ink-2)]">A room needs at least two guys, and you have {guys.length === 1 ? "one" : "none"} connected.</p>
        ) : (
          <>
            <div className="mt-4 max-h-[264px] space-y-0.5 overflow-y-auto">
              {guys.map((g) => {
                const on = picked.includes(g.uid);
                return (
                  <div key={g.uid} className={`flex items-center gap-2.5 rounded-xl px-2 py-1.5 ${on ? "bg-[var(--color-chip)]" : ""}`}>
                    <button className="flex min-w-0 flex-1 items-center gap-2.5 text-left" onClick={() => toggle(g.uid)} role="checkbox" aria-checked={on}>
                      <span
                        className={`flex h-[18px] w-[18px] shrink-0 items-center justify-center rounded-md border ${
                          on ? "border-[var(--color-accent)] bg-[var(--color-accent)] text-white" : "border-[var(--color-ink-3)]/60"
                        }`}
                      >
                        {on && <Check size={12} strokeWidth={3} />}
                      </span>
                      <Avatar agent={g} size={30} />
                      <span className="min-w-0">
                        <span className="block truncate text-[13px] font-medium">{guyName(g)}</span>
                        {multi && <span className="block truncate text-[11.5px] text-[var(--color-ink-3)]">{g.gatewayName}</span>}
                      </span>
                    </button>
                    {on && (
                      <button
                        onClick={() => setLead(g.uid)}
                        className={`shrink-0 rounded-full px-2 py-0.5 text-[11.5px] font-medium transition ${
                          leadUid === g.uid ? "bg-[var(--color-accent-soft)] text-[var(--color-accent-strong)]" : "text-[var(--color-ink-3)] hover:bg-[var(--color-chip)]"
                        }`}
                      >
                        {leadUid === g.uid ? "Leads" : "Make lead"}
                      </button>
                    )}
                  </div>
                );
              })}
            </div>
            <div className="mt-4">
              <Field label="Name" hint="Optional: without one, it's named after its guys.">
                <input
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && picked.length >= 2 && !busy && create()}
                  placeholder="Launch"
                  className={inputClass}
                />
              </Field>
            </div>
          </>
        )}
        {error && <p className="mt-3 text-[12px] text-[var(--color-bad)]">{error}</p>}
        <div className="mt-5 flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={picked.length < 2 || busy} onClick={create}>
            {picked.length < 2 ? "Pick at least two" : "Make the room"}
          </Button>
        </div>
      </div>
    </Modal>
  );
}
