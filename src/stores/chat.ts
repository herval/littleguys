// Async orchestration only; event reconciliation is deterministic and tested separately.
import { create } from "zustand";
import { chat as chatApi, sessions as sessionsApi, type Delivery } from "../lib/gateway";
import { decodeChatEvent } from "../lib/chatEvents";
import { errorMessage, listen } from "../lib/ipc";
import type { Attachment, ChatMessage } from "../lib/types";
import { blank, reduceThread, type Action, type ThreadState } from "./chatReducer";
export { waitingCount, type LiveRun, type ToolState, type ThreadState } from "./chatReducer";

export function threadId(gateway: string, key: string): string { return `${gateway}|${key}`; }
function splitThreadId(id: string): [string, string] {
  const i = id.indexOf("|");
  return [id.slice(0, i), id.slice(i + 1)];
}
interface ChatState {
  threads: Record<string, ThreadState>;
  open(id: string): Promise<void>;
  reload(id: string): Promise<void>;
  /**
   * `cwd`: folder for a thread's first message (ACP agents); `worktree`: start it in a new worktree (coder mode).
   * `delivery`: while the guy works, wait for its turn to end (`queue`) or go into it (`steer`).
   */
  send(id: string, text: string, attachments: Attachment[], cwd?: string, worktree?: boolean, delivery?: Delivery): Promise<void>;
  /** A queued message, now (`steer`: into the turn under way). */
  sendQueued(id: string, queuedId: string, delivery?: Delivery): Promise<void>;
  /** Takes a queued message back. */
  unqueue(id: string, queuedId: string): Promise<void>;
  abort(id: string): Promise<void>;
  handleEvent(gateway: string, name: string, payload: Record<string, unknown>): void;
  disconnect(gateway: string): void;
  resubscribe(gateway: string): void;
}
export function createChatStore(api = chatApi, subscribe = sessionsApi.subscribeMessages) {
  let request = 0;
  return create<ChatState>((set, get) => {
    const dispatch = (key: string, action: Action) => set((s) => ({
      threads: { ...s.threads, [key]: reduceThread(s.threads[key] ?? blank(key), action) },
    }));
    const keys = (gateway: string) => Object.keys(get().threads).filter((key) => splitThreadId(key)[0] === gateway);
    return {
      threads: {},
      async open(key) {
        const [gw, sessionKey] = splitThreadId(key);
        subscribe(gw, sessionKey).catch((e) => dispatch(key, { type: "error", error: errorMessage(e) }));
        if (!get().threads[key]?.loaded) await get().reload(key);
      },
      async reload(key) {
        const ticket = ++request;
        dispatch(key, { type: "load", request: ticket });
        const epoch = get().threads[key].epoch;
        const [gw, sessionKey] = splitThreadId(key);
        try { dispatch(key, { type: "history", request: ticket, epoch, history: await api.history(gw, sessionKey), now: Date.now() }); }
        catch (e) { dispatch(key, { type: "loadError", request: ticket, epoch, error: errorMessage(e) }); }
      },
      async send(key, text, attachments, cwd, worktree, delivery) {
        const id = crypto.randomUUID();
        const message: ChatMessage = { role: "user", content: [
          ...(text ? [{ type: "text" as const, text }] : []),
          ...attachments.filter((a) => a.type === "image").map((a) => ({ type: "image" as const, url: a.previewUrl, mimeType: a.mimeType })),
        ], timestamp: Date.now(), __openclaw: { clientSendId: id } };
        dispatch(key, { type: "send", pending: { idempotencyKey: id, message, status: "sending" } });
        const [gw, sessionKey] = splitThreadId(key);
        try {
          const receipt = await api.send(gw, sessionKey, text, attachments, cwd, id, worktree, delivery);
          dispatch(key, { type: "receipt", id, receipt });
        } catch (e) {
          const code = e && typeof e === "object" && "code" in e ? String(e.code) : "";
          const uncertain = !["INVALID_REQUEST", "NOT_FOUND", "UNAVAILABLE", "UNAUTHORIZED", "FORBIDDEN", "BUSY"].includes(code);
          dispatch(key, { type: "sendError", id, uncertain, error: uncertain ?
            `Delivery could not be confirmed: ${errorMessage(e)}. Check the conversation before sending again.` : errorMessage(e) });
          // Leave uncertain sends visible. Do not restore the composer and invite an accidental resend.
          if (!uncertain) throw e;
        }
      },
      async sendQueued(key, queuedId, delivery) {
        const [gw, sessionKey] = splitThreadId(key);
        // Its chat.queue events put it in the thread (or back in the queue).
        try { await api.sendQueued(gw, sessionKey, queuedId, delivery); }
        catch (e) { dispatch(key, { type: "error", error: errorMessage(e) }); }
      },
      async unqueue(key, queuedId) {
        const [gw, sessionKey] = splitThreadId(key);
        dispatch(key, { type: "unqueue", id: queuedId });
        await api.unqueue(gw, sessionKey, queuedId).catch(() => {});
      },
      async abort(key) {
        const [gw, sessionKey] = splitThreadId(key);
        try { await api.abort(gw, sessionKey, get().threads[key]?.live?.runId); }
        catch (e) { dispatch(key, { type: "error", error: `Couldn't stop the run: ${errorMessage(e)}` }); }
      },
      handleEvent(gateway, name, payload) {
        if (typeof payload.sessionKey !== "string") return;
        const key = threadId(gateway, payload.sessionKey);
        const t = get().threads[key];
        const event = decodeChatEvent(name, payload);
        if (!t || !event) return;
        dispatch(key, { type: "event", event, now: Date.now() });
        if (event.kind === "run" && ["final", "error", "aborted"].includes(event.state) && !t.retired.includes(event.runId)) {
          const epoch = t.epoch;
          setTimeout(() => { if (get().threads[key]?.epoch === epoch) void get().reload(key); }, 250);
        }
      },
      disconnect(gateway) { keys(gateway).forEach((key) => dispatch(key, { type: "disconnect" })); },
      resubscribe(gateway) {
        for (const key of keys(gateway)) {
          const [, sessionKey] = splitThreadId(key);
          subscribe(gateway, sessionKey).catch((e) => dispatch(key, { type: "error", error: errorMessage(e) }));
          void get().reload(key);
        }
      },
    };
  });
}
export const useChat = createChatStore();
let wired = false;
export async function connectChatStore() {
  if (wired) return;
  wired = true;
  await listen<{ gateway: string; event: string; payload: Record<string, unknown> }>("gw:event", (frame) => {
    useChat.getState().handleEvent(frame.gateway, frame.event, frame.payload ?? {});
  });
  const lastPhase = new Map<string, string>();
  await listen<{ gatewayId: string; phase: string }>("gw:status", (status) => {
    const prev = lastPhase.get(status.gatewayId);
    if (status.phase !== "connected" && prev === "connected") useChat.getState().disconnect(status.gatewayId);
    if (status.phase === "connected" && prev !== "connected") useChat.getState().resubscribe(status.gatewayId);
    lastPhase.set(status.gatewayId, status.phase);
  });
}
