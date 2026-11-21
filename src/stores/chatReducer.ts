import type { ChatMessage, QueuedMessage } from "../lib/types";
import type { ChatEvent, SendReceipt } from "../lib/chatEvents";
import type { ChatHistory } from "../lib/gateway";

export interface LiveRun { seq?: number; runId: string; text: string; phase: string | null; startedAt: number }
export type ToolState = "running" | "completed" | "error";
export interface PendingSend {
  idempotencyKey: string;
  runId?: string;
  status: "sending" | "queued" | "unknown";
  message: ChatMessage;
  /** Native handoff accepted; keep the bubble until its transcript arrives.
   * `since`: the earliest transcript time that can be it (a remote gateway's clock may run behind this Mac's). */
  delivery?: { ids: string[]; texts: string[]; since?: number };
  /** Transcript rows that predate this send or already confirmed another batch. */
  seen?: string[];
}
export interface ThreadState {
  key: string;
  loading: boolean;
  loaded: boolean;
  error: string | null;
  messages: ChatMessage[];
  hasMore: boolean;
  live: LiveRun | null;
  toolStatus: Record<string, ToolState>;
  pending: PendingSend[];
  /** Written while the guy worked: littleguys sends them when its turn ends (outbox.rs). */
  queued: QueuedMessage[];
  sendError: string | null;
  /** Its first message is waiting for you to trust this folder for Claude Code. */
  untrustedFolder: string | null;
  epoch: number;
  request: number;
  /** Events not yet seen in a history snapshot. */
  observed: ChatMessage[];
  /** Settled events present when the current history request started. */
  historyObserved: ChatMessage[];
  /** Completed or superseded run IDs; late events must not revive them. */
  retired: string[];
}
export type Action =
  | { type: "load"; request: number }
  | { type: "history"; request: number; epoch: number; history: ChatHistory; now: number }
  | { type: "loadError"; request: number; epoch: number; error: string }
  | { type: "send"; pending: PendingSend }
  | { type: "receipt"; id: string; receipt: SendReceipt }
  | { type: "sendError"; id: string; error: string; uncertain: boolean }
  | { type: "error"; error: string }
  /** You trusted the folder; its waiting message is starting the thread. */
  | { type: "trusted" }
  /** You took a queued message back (littleguys confirms with a `queue` event). */
  | { type: "unqueue"; id: string }
  | { type: "event"; event: ChatEvent; now: number }
  | { type: "disconnect" };

export const blank = (key: string): ThreadState => ({
  key, loading: false, loaded: false, error: null, messages: [], hasMore: false, live: null,
  toolStatus: {}, pending: [], queued: [], sendError: null, untrustedFolder: null, epoch: 0, request: 0, observed: [], historyObserved: [], retired: [],
});
export function textOf(message: ChatMessage | undefined): string {
  if (!message) return "";
  return typeof message.content === "string" ? message.content : message.content
    .filter((b): b is { type: "text"; text: string } => b?.type === "text" && typeof b.text === "string")
    .map((b) => b.text).join("");
}
function identity(m: ChatMessage): string {
  return m.__openclaw?.id ?? JSON.stringify([m.role, m.timestamp, m.content, m.toolCallId]);
}
function merge(base: ChatMessage[], extra: ChatMessage[]): ChatMessage[] {
  const result = [...base];
  const indices = new Map(result.map((m, i) => [identity(m), i]));
  for (const m of extra) {
    const i = indices.get(identity(m));
    if (i !== undefined) result[i] = m;
    else { indices.set(identity(m), result.length); result.push(m); }
  }
  return result;
}
/** OpenClaw keeps a message from chat.send as `<idempotencyKey>:user` and puts that on its
 * transcript row (`session.message` and history alike); it never echoes `clientSendId`, and
 * gives a user row no `runId`.
 */
function openclawSendId(m: ChatMessage): string | undefined {
  const key = m.__openclaw?.idempotencyKey;
  return key?.endsWith(":user") ? key.slice(0, -":user".length) : undefined;
}
/** The send this row says it came from, if it says (it may be another client's). */
function sendIdOf(m: ChatMessage): string | undefined {
  return m.__openclaw?.clientSendId ?? openclawSendId(m);
}
function acknowledged(p: PendingSend, m: ChatMessage): boolean {
  if (m.role !== "user") return false;
  const sent = sendIdOf(m);
  if (sent) return sent === p.idempotencyKey;
  return Boolean(p.runId && m.__openclaw?.runId === p.runId);
}
const GATEWAY_CLOCK_SKEW_MS = 60_000;
/** What a send's receipt says was handed over, for matching its transcript row by text. A native
 * runtime says `delivery`; a gateway that names no send ID (OpenClaw) took it once it answers,
 * in case its row doesn't name it either (gateways older than `:user` keys).
 */
function handoff(p: PendingSend, id: string, receipt: SendReceipt): PendingSend["delivery"] {
  const texts = [textOf(p.message).trim()];
  if (receipt.delivery === "delivered") return { ids: [id], texts };
  if (receipt.delivery || receipt.clientSendId || p.message.timestamp === undefined) return undefined;
  return { ids: [id], texts, since: p.message.timestamp - GATEWAY_CLOCK_SKEW_MS };
}
/** Native CLIs cannot echo client IDs. Match only an accepted handoff, once,
 * against a new transcript row at or after the send (timestamps may round to seconds).
 * Explicit correlation always wins, including messages from another client.
 */
function reconcilePending(pending: PendingSend[], messages: ChatMessage[]): PendingSend[] {
  let remaining = pending;
  for (const m of messages) {
    if (m.role !== "user") continue;
    const matched = remaining.filter((p) => acknowledged(p, m));
    if (!matched.length && !sendIdOf(m)) {
      const p = remaining.find((p) => p.delivery && !p.seen?.includes(identity(m)) &&
        m.timestamp !== undefined && p.message.timestamp !== undefined &&
        m.timestamp >= (p.delivery.since ?? Math.floor(p.message.timestamp / 1000) * 1000) &&
        p.delivery.texts.includes(textOf(m).trim()));
      if (p) matched.push(...remaining.filter((other) => p.delivery!.ids.includes(other.idempotencyKey)));
    }
    if (matched.length) {
      remaining = remaining.filter((p) => !matched.includes(p))
        .map((p) => ({ ...p, seen: [...(p.seen ?? []), identity(m)] }));
    }
  }
  return remaining;
}
function retire(ids: string[], id: string): string[] {
  return ids.includes(id) ? ids : [...ids, id].slice(-2048);
}
export function reduceThread(t: ThreadState, a: Action): ThreadState {
  switch (a.type) {
    case "load": return { ...t, loading: true, error: null, request: a.request,
      historyObserved: t.observed.filter((m) => !t.live || m.__openclaw?.runId !== t.live.runId) };
    case "history": {
      if (a.request !== t.request || a.epoch !== t.epoch) return t;
      const canonical = a.history.messages ?? [];
      const ids = new Set(canonical.map(identity));
      // The snapshot replaces the settled stream overlay from before the request.
      // In particular, Hermes uses different IDs for live segments and stored rows.
      const observed = t.observed.filter((m) => !ids.has(identity(m)) && !t.historyObserved.includes(m));
      const messages = merge(canonical, observed);
      const run = a.history.inFlightRun;
      const live = t.live ?? (run?.runId && !t.retired.includes(run.runId) ?
        { runId: run.runId, text: textOf(run.message), phase: null, startedAt: a.now } : null);
      return { ...t, messages, observed, historyObserved: [], live, loading: false, loaded: true, hasMore: Boolean(a.history.hasMore),
        pending: reconcilePending(t.pending, messages), queued: a.history.queued ?? t.queued };
    }
    case "loadError": return a.request === t.request && a.epoch === t.epoch ? { ...t, loading: false, error: a.error } : t;
    case "send": return { ...t, pending: [...t.pending, { ...a.pending, seen: t.messages.map(identity) }], sendError: null, untrustedFolder: null };
    case "receipt": {
      // A send response is an acknowledgement, never evidence of a live run.
      const pending = t.pending.map((p) => p.idempotencyKey === a.id ? {
        ...p, runId: a.receipt.runId, status: "queued" as const,
        delivery: p.delivery ?? handoff(p, a.id, a.receipt),
      } : p).filter((p) => !(p.idempotencyKey === a.id && (a.receipt.status === "ok" || a.receipt.status === "held")));
      return { ...t, pending: reconcilePending(pending, t.messages) };
    }
    case "sendError":
      if (!t.pending.some((p) => p.idempotencyKey === a.id && !p.delivery)) return t;
      return { ...t, sendError: a.error, pending: a.uncertain ?
      t.pending.map((p) => p.idempotencyKey === a.id ? { ...p, status: "unknown" } : p) :
      t.pending.filter((p) => p.idempotencyKey !== a.id) };
    case "error": return { ...t, sendError: a.error };
    // What waited was never handed over; it's being sent now.
    case "trusted": return { ...t, untrustedFolder: null, sendError: null,
      pending: t.pending.map((p) => p.status === "unknown" && !p.delivery ? { ...p, status: "sending" as const } : p) };
    case "unqueue": return { ...t, queued: t.queued.filter((q) => q.id !== a.id) };
    case "disconnect": return { ...t, epoch: t.epoch + 1, loading: false, live: null, toolStatus: {},
      pending: t.pending.map((p) => p.delivery ? p : { ...p, status: "unknown" }) };
    case "event": {
      const e = a.event;
      if (e.kind === "tool") {
        // A delayed start cannot reopen a completed tool call.
        if (t.toolStatus[e.id] && t.toolStatus[e.id] !== "running" && e.status === "running") return t;
        return { ...t, toolStatus: { ...t.toolStatus, [e.id]: e.status } };
      }
      if (e.kind === "queue") {
        // Handed over by littleguys, not this window: it shows as sending, like one you just wrote.
        let next: ThreadState = { ...t, queued: e.queued, sendError: e.error ?? t.sendError };
        if (e.sent && !t.pending.some((p) => p.idempotencyKey === e.sent!.id)) {
          const message: ChatMessage = { role: "user", content: e.sent.text ? [{ type: "text", text: e.sent.text }] : [],
            timestamp: a.now, __openclaw: { clientSendId: e.sent.id } };
          next = reduceThread(next, { type: "send", pending: { idempotencyKey: e.sent.id, message, status: "sending" } });
          next = { ...next, sendError: e.error ?? t.sendError };
        }
        if (e.accepted) next = reduceThread(next, { type: "receipt", id: e.accepted.id, receipt: e.accepted.receipt });
        if (e.unsent) next = { ...next, pending: next.pending.filter((p) => p.idempotencyKey !== e.unsent) };
        return next;
      }
      if (e.kind === "delivery") {
        if (!t.pending.some((p) => e.ids.includes(p.idempotencyKey))) return t;
        const batch = e.ids.map((id) => t.pending.find((p) => p.idempotencyKey === id)).filter((p): p is PendingSend => Boolean(p));
        const texts = [batch.map((p) => textOf(p.message).trim()).join("\n\n"), ...(e.text ? [e.text.trim()] : [])];
        const pending = t.pending.map((p) => !e.ids.includes(p.idempotencyKey) || p.delivery ? p :
          e.status === "delivered" ? { ...p, status: "queued" as const, delivery: { ids: e.ids, texts } } : { ...p, status: "unknown" as const });
        const uncertain = pending.some((p) => e.ids.includes(p.idempotencyKey) && !p.delivery);
        return { ...t, pending: reconcilePending(pending, t.messages),
          sendError: e.status === "unknown" && uncertain ? e.error ?? "Delivery could not be confirmed. Check the conversation before sending again." : t.sendError };
      }
      if (e.kind === "message") {
        const m = { ...e.message, __openclaw: { ...e.message.__openclaw, ...(e.runId ? { runId: e.runId } : {}),
          ...(e.clientSendId ? { clientSendId: e.clientSendId } : {}) } };
        return { ...t, messages: merge(t.messages, [m]), observed: merge(t.observed, [m]),
          pending: reconcilePending(t.pending, [m]),
          live: m.role === "assistant" && t.live && e.runId === t.live.runId && textOf(m) ? { ...t.live, text: "" } : t.live };
      }
      if (["final", "error", "aborted"].includes(e.state)) {
        return { ...t, retired: retire(t.retired, e.runId), live: t.live?.runId === e.runId ? null : t.live,
          // A folder to trust says it all (with a button); Claude Code's own words about it would only confuse.
          sendError: e.state === "error" && e.untrustedFolder ? null :
            e.state === "error" && (!t.live || t.live.runId === e.runId) && !t.retired.includes(e.runId) ?
            e.error ?? "The run failed." : t.sendError,
          untrustedFolder: e.state === "error" && e.untrustedFolder ? e.untrustedFolder : t.untrustedFolder };
      }
      if (t.retired.includes(e.runId)) return t;
      if (t.live && t.live.runId !== e.runId && e.state === "delta") return t;
      if (t.live?.runId === e.runId && e.seq !== undefined && t.live.seq !== undefined && e.seq <= t.live.seq) return t;
      const current = t.live?.runId === e.runId ? t.live : { runId: e.runId, text: "", phase: null, startedAt: a.now };
      const live = { ...current, seq: e.seq ?? current.seq };
      return { ...t, retired: t.live && t.live.runId !== e.runId ? retire(t.retired, t.live.runId) : t.retired,
        live: e.state === "status" ? { ...live, phase: e.phase ?? live.phase } :
          { ...live, phase: null, text: textOf(e.message) || live.text + (e.deltaText ?? "") } };
    }
  }
}
export function waitingCount(t: ThreadState | undefined): number {
  return t?.pending.filter((p) => p.status !== "unknown" && !(t.live && p.runId === t.live.runId)).length ?? 0;
}
