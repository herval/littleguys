// Rooms: chats with several guys in them (docs/GROUP-CHATS.md). littleguys runs them
// (src-tauri/src/runtime/rooms); here a room is a guy on the "rooms" gateway whose row
// says who's in it (`guy.room`).
import { create } from "zustand";
import { findGuy, ROOMS_GATEWAY } from "./guys";
import { gwCall, listen } from "./ipc";
import type { Attention, ChatMessage, Guy, RoomInfo, RoomMember, RoomState, Roster } from "./types";

export const rooms = {
  create: (p: { name?: string; members: string[]; lead?: string }) => gwCall<{ id: string; key: string }>(ROOMS_GATEWAY, "rooms.create", p),
  update: (id: string, p: { name?: string; lead?: string; add?: string[]; remove?: string[]; attention?: Record<string, Attention>; folder?: Record<string, string | null> }) =>
    gwCall<{ ok: boolean }>(ROOMS_GATEWAY, "rooms.update", { id, ...p }),
  remove: (id: string) => gwCall<{ ok: boolean }>(ROOMS_GATEWAY, "rooms.delete", { id }),
  /** After a pause: the guys go on. */
  resume: (id: string) => gwCall<{ ok: boolean }>(ROOMS_GATEWAY, "rooms.continue", { id }),
  state: (id: string) => gwCall<RoomState>(ROOMS_GATEWAY, "rooms.state", { id }),
};

/** A member with the guy it is, when its bot is connected. */
export interface Seat extends RoomMember {
  guy?: Guy;
  lead: boolean;
}

export function seatsOf(roster: Roster, room: RoomInfo): Seat[] {
  return room.members.map((m) => {
    const guy = findGuy(roster, m.uid);
    return { ...m, guy, lead: m.uid === room.lead };
  });
}

/** What a member's avatar is drawn from: the guy itself, or its name while it's away. */
export function seatFace(seat: Pick<Seat, "uid" | "name" | "guy">): Pick<Guy, "id" | "name" | "identity"> {
  return seat.guy ?? { id: seat.uid, name: seat.name, identity: { name: seat.name } };
}

/**
 * Who a message that names nobody goes to: whoever spoke last (you're answering them),
 * else the lead. The same rule as the room's own (rooms/mod.rs, `send`).
 */
export function defaultTo(room: RoomInfo, messages: ChatMessage[]): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    const from = messages[i].room?.from;
    if (from && room.members.some((m) => m.uid === from)) return from;
  }
  return room.members.some((m) => m.uid === room.lead) ? room.lead : (room.members[0]?.uid ?? room.lead);
}

const wordChar = /[\p{L}\p{N}_]/u;

/** The members a text addresses with `@handle` or `@Name`, and whether it says `@everyone`. */
export function mentioned(text: string, members: RoomMember[]): string[] {
  const lower = text.toLowerCase();
  const found: string[] = [];
  for (let at = lower.indexOf("@"); at !== -1; at = lower.indexOf("@", at + 1)) {
    if (at > 0 && wordChar.test(lower[at - 1])) continue;
    const after = lower.slice(at + 1);
    const ends = (len: number) => !wordChar.test(after[len] ?? "");
    if (["everyone", "all"].some((w) => after.startsWith(w) && ends(w.length))) {
      members.forEach((m) => !found.includes(m.uid) && found.push(m.uid));
      continue;
    }
    let best: { uid: string; len: number } | null = null;
    for (const m of members) {
      for (const name of [m.handle.toLowerCase(), m.name.toLowerCase()]) {
        if (name && after.startsWith(name) && ends(name.length) && (!best || name.length > best.len)) best = { uid: m.uid, len: name.length };
      }
    }
    if (best && !found.includes(best.uid)) found.push(best.uid);
  }
  return found;
}

/** The `@word` being typed at the caret, if any: where it starts and what's typed so far. */
export function mentionAt(text: string, caret: number): { start: number; query: string } | null {
  const before = text.slice(0, caret);
  const at = before.lastIndexOf("@");
  if (at === -1 || (at > 0 && wordChar.test(before[at - 1]))) return null;
  const query = before.slice(at + 1);
  return /^[\p{L}\p{N}_-]*$/u.test(query) ? { start: at, query } : null;
}

/** `@handle`s in a message, in bold, for the transcript. */
export function boldMentions(text: string, members: RoomMember[]): string {
  const handles = [...members.map((m) => m.handle), "everyone"].filter(Boolean).sort((a, b) => b.length - a.length);
  if (!handles.length) return text;
  const escaped = handles.map((h) => h.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  return text.replace(new RegExp(`(^|[^\\p{L}\\p{N}_*])@(${escaped.join("|")})(?![\\p{L}\\p{N}_])`, "giu"), "$1**@$2**");
}

interface RoomsState {
  /** By room id. */
  states: Record<string, RoomState>;
  set(state: RoomState): void;
}

export const useRooms = create<RoomsState>((set) => ({
  states: {},
  set: (state) => set((s) => ({ states: { ...s.states, [state.id]: state } })),
}));

/** Who's working in a room, kept up to date. Call once per window. */
export async function connectRoomsStore() {
  await listen<{ gateway: string; event: string; payload: RoomState }>("gw:event", (frame) => {
    if (frame.gateway === ROOMS_GATEWAY && frame.event === "room.state" && frame.payload?.id) useRooms.getState().set(frame.payload);
  });
}

/** "Patch is working · Vera is reading", for a room's status line. */
export function workingLine(state: RoomState | undefined, seats: Seat[]): string | null {
  if (!state?.working.length) return null;
  const name = (uid: string) => seats.find((s) => s.uid === uid)?.name ?? "someone";
  const parts = state.working.map((w) => `${name(w.uid)} is ${w.why === "look" ? "reading" : "working"}`);
  return parts.join(" · ");
}
