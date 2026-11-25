// The rooms runtime (src-tauri/src/runtime/rooms), faked for `pnpm dev` in a browser: one
// room with a conversation in it, and guys that answer from a script so turn-taking shows.
import type { Attention, ChatMessage, ConnStatus, GatewayRoster, RoomInfo, RoomState, Roster } from "./types";

interface MockRoom {
  id: string;
  name: string;
  info: RoomInfo;
  messages: ChatMessage[];
  working: RoomState["working"];
  run: string | null;
  timers: ReturnType<typeof setTimeout>[];
}

const NO_CAPS = { manageGuys: false, routines: false, routinesLocal: false, approvals: false, threadActions: false, newThreads: false, attachments: false, models: false, files: false, editFiles: false, skills: false, commands: false, tools: false, plugins: false, browser: false, pastSessions: false, code: false, steer: false };

export function mockRooms(roster: Roster, emit: (event: string, payload: unknown) => void, base: ConnStatus, now: number) {
  const min = 60_000;
  const [JEFE, CLAUDE, OCTO] = ["openclaw/main", "acp-claude/bot", "gw-garage1/main"];
  const said = (id: string, at: number, text: string, from: string | null, to: string[]): ChatMessage => ({
    role: from ? "assistant" : "user",
    content: [{ type: "text", text }],
    timestamp: at,
    room: { from, to },
    __openclaw: { id },
  });
  const rooms: MockRoom[] = [
    {
      id: "room-launch",
      name: "Launch",
      info: {
        lead: JEFE,
        members: [
          { uid: JEFE, name: "Jefe", handle: "Jefe", attention: "oncall", seat: null },
          { uid: CLAUDE, name: "Claude", handle: "Claude", attention: "oncall", seat: "agent:bot:dashboard:room-launch" },
          { uid: OCTO, name: "Octo", handle: "Octo", attention: "listening", seat: null },
        ],
      },
      messages: [
        said("l1", now - 9 * min, "signups are failing on Safari since this morning", null, [JEFE]),
        said("l2", now - 8 * min, "It's the date parser: Safari rejects the format we send. @Claude it's in `signup/validate.ts`, can you fix it?", JEFE, [CLAUDE]),
        said("l3", now - 5 * min, "Fixed and pushed, with a test for Safari's format.", CLAUDE, []),
        said("l4", now - 4 * min, "The same parser is used in the invite flow, which will fail the same way. @Claude", OCTO, [CLAUDE]),
        said("l5", now - 2 * min, "Good catch, fixed there too.", CLAUDE, []),
      ],
      working: [],
      run: null,
      timers: [],
    },
  ];

  const key = (id: string) => `agent:${id}:main`;
  const find = (id: unknown) => {
    const room = rooms.find((r) => r.id === id || key(r.id) === id);
    if (!room) throw Object.assign(new Error("That room is gone."), { code: "NOT_FOUND" });
    return room;
  };
  const text = (m: ChatMessage) => (typeof m.content === "string" ? m.content : m.content.map((b) => ("text" in b ? String(b.text) : "")).join(""));
  const nameOf = (room: MockRoom, uid: string) => room.info.members.find((m) => m.uid === uid)?.name ?? "someone";

  const gateway: GatewayRoster = {
    id: "rooms",
    name: "Rooms",
    enabled: true,
    status: { ...base, gatewayId: "rooms", gatewayName: "Rooms", kind: "rooms", mode: "local", url: null, phase: "connected", capabilities: NO_CAPS, hello: null },
    loaded: true,
    defaultAgentId: null,
    agents: [],
    sessions: [],
    running: [],
    approvals: [],
  };

  /** The roster's rows for the rooms as they are now, sent to every window. */
  const publish = () => {
    gateway.agents = rooms.map((r) => ({ id: r.id, name: r.name, identity: { name: r.name }, room: r.info })) as never;
    gateway.sessions = rooms.map((r) => {
      const last = r.messages[r.messages.length - 1];
      const from = last?.room?.from;
      return { key: key(r.id), agentId: r.id, kind: "group", updatedAt: last?.timestamp ?? now, lastMessagePreview: last ? `${from ? `${nameOf(r, from)}: ` : ""}${text(last)}` : null };
    }) as never;
    gateway.running = rooms.filter((r) => r.run).map((r) => key(r.id));
    emit("lg:roster", { ...roster, gateways: [...roster.gateways] });
  };
  const event = (room: MockRoom, name: string, payload: object) => emit("gw:event", { gateway: "rooms", event: name, payload: { sessionKey: key(room.id), ...payload } });
  const state = (room: MockRoom): RoomState => ({ id: room.id, sessionKey: key(room.id), working: room.working, paused: false });
  const work = (room: MockRoom, working: RoomState["working"]) => {
    room.working = working;
    event(room, "room.state", state(room));
  };
  const post = (room: MockRoom, message: ChatMessage) => {
    room.messages.push(message);
    event(room, "session.message", { runId: room.run, message });
    publish();
  };
  const later = (room: MockRoom, ms: number, fn: () => void) => room.timers.push(setTimeout(fn, ms));

  /** The floor, in small: whoever is addressed answers; the lead brings in the next guy; listeners look last. */
  const turn = (room: MockRoom, uid: string, why: "addressed" | "lead", asked: string, hops: number) => {
    work(room, [{ uid, why }]);
    later(room, 1600, () => {
      const others = room.info.members.filter((m) => m.uid !== uid);
      const next = uid === room.info.lead && hops === 0 ? others.find((m) => m.attention === "oncall") : undefined;
      const reply = next
        ? `On it. @${next.handle} can you take the part about “${asked.slice(0, 40)}”? (mock)`
        : `Done: “${asked.slice(0, 60)}” is handled. This is the mock backend. (mock)`;
      post(room, said(crypto.randomUUID(), Date.now(), reply, uid, next ? [next.uid] : []));
      if (next) return turn(room, next.uid, "addressed", asked, hops + 1);
      const listeners = room.info.members.filter((m) => m.attention === "listening" && m.uid !== uid);
      if (!listeners.length) return quiet(room);
      // One look each, one at a time, and (here) nothing to add.
      work(room, [{ uid: listeners[0].uid, why: "look" }]);
      later(room, 1400, () => quiet(room));
    });
  };
  const quiet = (room: MockRoom) => {
    const run = room.run;
    room.run = null;
    work(room, []);
    event(room, "chat", { runId: run, state: "final" });
    publish();
  };

  publish();

  return {
    gateway,
    async call(method: string, p: Record<string, unknown>): Promise<unknown> {
      switch (method) {
        case "sessions.subscribe":
        case "sessions.messages.subscribe":
          return { subscribed: true };
        case "sessions.list":
          return { sessions: p.archived === true ? [] : gateway.sessions };
        case "sessions.patch":
          return { ok: true, key: p.key };
        case "chat.history": {
          const room = find(p.sessionKey);
          return { sessionKey: p.sessionKey, messages: room.messages, hasMore: false, inFlightRun: room.run ? { runId: room.run } : null };
        }
        case "rooms.state":
          return state(find(p.id));
        case "chat.send": {
          const room = find(p.sessionKey);
          const to = Array.isArray(p.to) && p.to.length ? (p.to as string[]) : [room.info.lead];
          const opened = !room.run;
          room.run ??= crypto.randomUUID();
          const message = said(crypto.randomUUID(), Date.now(), String(p.message), null, to);
          message.__openclaw = { ...message.__openclaw, clientSendId: p.idempotencyKey as string };
          room.messages.push(message);
          event(room, "session.message", { runId: room.run, clientSendId: p.idempotencyKey, message });
          if (opened) event(room, "chat", { runId: room.run, state: "status", phase: "room" });
          room.timers.forEach(clearTimeout);
          room.timers = [];
          publish();
          turn(room, to[0], Array.isArray(p.to) && p.to.length ? "addressed" : "lead", String(p.message), 0);
          return { runId: room.run, status: "started", clientSendId: p.idempotencyKey };
        }
        case "chat.abort": {
          const room = find(p.sessionKey);
          room.timers.forEach(clearTimeout);
          room.timers = [];
          const run = room.run;
          room.run = null;
          work(room, []);
          if (run) event(room, "chat", { runId: run, state: "aborted" });
          publish();
          return { ok: true, aborted: true };
        }
        case "rooms.create": {
          const uids = (p.members as string[]) ?? [];
          if (uids.length < 2) throw Object.assign(new Error("A room needs at least two guys."), { code: "INVALID_REQUEST" });
          const guys = roster.gateways.flatMap((g) => g.agents.map((a) => ({ uid: `${g.id}/${a.id}`, name: a.identity?.name ?? a.name ?? a.id })));
          const members = uids.map((uid) => {
            const name = guys.find((g) => g.uid === uid)?.name ?? uid;
            return { uid, name, handle: name.replace(/[^\p{L}\p{N}_-]/gu, ""), attention: "oncall" as Attention, seat: null };
          });
          const id = `room-${crypto.randomUUID().slice(0, 8)}`;
          rooms.push({ id, name: String(p.name ?? members.map((m) => m.name).join(", ")), info: { lead: String(p.lead ?? uids[0]), members }, messages: [], working: [], run: null, timers: [] });
          publish();
          return { ok: true, id, key: key(id) };
        }
        case "rooms.update": {
          const room = find(p.id);
          const info = { ...room.info, members: room.info.members.map((m) => ({ ...m })) };
          if (typeof p.name === "string") room.name = p.name;
          for (const [uid, mode] of Object.entries((p.attention as Record<string, Attention>) ?? {})) {
            const m = info.members.find((m) => m.uid === uid);
            if (m) m.attention = mode;
          }
          info.members = info.members.filter((m) => !((p.remove as string[]) ?? []).includes(m.uid));
          for (const uid of (p.add as string[]) ?? []) {
            const guy = roster.gateways.flatMap((g) => g.agents.map((a) => ({ uid: `${g.id}/${a.id}`, name: a.identity?.name ?? a.name ?? a.id }))).find((g) => g.uid === uid);
            if (guy && !info.members.some((m) => m.uid === uid)) info.members.push({ uid, name: guy.name, handle: guy.name.replace(/[^\p{L}\p{N}_-]/gu, ""), attention: "oncall", seat: null });
          }
          if (typeof p.lead === "string") info.lead = p.lead;
          if (!info.members.some((m) => m.uid === info.lead)) info.lead = info.members[0].uid;
          room.info = info;
          publish();
          return { ok: true };
        }
        case "rooms.delete": {
          const at = rooms.findIndex((r) => r.id === p.id);
          if (at !== -1) rooms.splice(at, 1);
          publish();
          return { ok: true };
        }
        case "rooms.continue":
          return { ok: true };
        default:
          throw Object.assign(new Error(`${method} isn't supported by this bot`), { code: "UNSUPPORTED" });
      }
    },
  };
}
