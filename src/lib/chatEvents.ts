import type { ChatMessage, QueuedMessage } from "./types";

/** Additive wire contract: legacy gateways can omit clientSendId and delivery. */
export interface SendReceipt {
  runId: string;
  status: string;
  clientSendId?: string;
  delivery?: "queued" | "delivered";
}
export type ChatEvent =
  | { kind: "message"; message: ChatMessage; runId?: string; clientSendId?: string }
  /** `untrustedFolder`: a new thread didn't start because Claude Code hasn't been told to trust that folder. */
  | { kind: "run"; runId: string; state: "status" | "delta" | "final" | "aborted" | "error"; seq?: number; phase?: string; message?: ChatMessage; deltaText?: string; error?: string;
      untrustedFolder?: string }
  | { kind: "delivery"; ids: string[]; status: "delivered" | "unknown"; text?: string; error?: string }
  | { kind: "tool"; id: string; status: "running" | "completed" | "error" }
  /** What littleguys holds for the thread changed (src-tauri/src/outbox.rs). `sent`: it's handing
   * that over now; `accepted`: what chat.send answered for it; `unsent`: that failed, it waits again. */
  | { kind: "queue"; queued: QueuedMessage[]; sent?: { id: string; text: string }; accepted?: { id: string; receipt: SendReceipt };
      unsent?: string; error?: string };

const str = (v: unknown) => typeof v === "string" ? v : undefined;
const record = (v: unknown): Record<string, unknown> | undefined => v && typeof v === "object" && !Array.isArray(v) ? v as Record<string, unknown> : undefined;
function message(v: unknown): ChatMessage | undefined {
  const m = record(v);
  return m && typeof m.role === "string" && (typeof m.content === "string" || Array.isArray(m.content)) ? m as unknown as ChatMessage : undefined;
}
/** Unknown events and malformed envelopes never enter the state machine. */
export function decodeChatEvent(name: string, p: Record<string, unknown>): ChatEvent | null {
  if (name === "session.message") {
    const m = message(p.message);
    return m ? { kind: "message", message: m, runId: str(p.runId) ?? m.__openclaw?.runId, clientSendId: str(p.clientSendId) ?? m.__openclaw?.clientSendId } : null;
  }
  if (name === "chat" && str(p.runId) && ["status", "delta", "final", "aborted", "error"].includes(String(p.state))) {
    return { kind: "run", runId: p.runId as string, state: p.state as "status" | "delta" | "final" | "aborted" | "error",
      seq: typeof p.seq === "number" ? p.seq : undefined, phase: str(p.phase), message: message(p.message), deltaText: str(p.deltaText), error: str(p.errorMessage),
      ...(str(p.untrustedFolder) ? { untrustedFolder: str(p.untrustedFolder) } : {}) };
  }
  if (name === "chat.delivery" && Array.isArray(p.clientSendIds) && ["delivered", "unknown"].includes(String(p.status))) {
    return { kind: "delivery", ids: p.clientSendIds.filter((id): id is string => typeof id === "string"), status: p.status as "delivered" | "unknown", text: str(p.text), error: str(p.errorMessage) };
  }
  if (name === "chat.queue" && Array.isArray(p.queued)) {
    const sent = record(p.sent), accepted = record(p.accepted), receipt = record(accepted?.receipt);
    return { kind: "queue", queued: p.queued as QueuedMessage[],
      sent: sent && str(sent.id) ? { id: sent.id as string, text: str(sent.text) ?? "" } : undefined,
      accepted: accepted && str(accepted.id) && receipt && str(receipt.runId) && str(receipt.status) ?
        { id: accepted.id as string, receipt: receipt as unknown as SendReceipt } : undefined,
      unsent: str(p.unsent), error: str(p.error) };
  }
  const data = record(p.data);
  if (name === "agent" && data && (data.kind === "tool" || p.stream === "tool") && str(data.toolCallId)) {
    return { kind: "tool", id: data.toolCallId as string, status: data.phase === "start" || data.phase === "update" ? "running" : data.isError || data.status === "failed" ? "error" : "completed" };
  }
  return null;
}
