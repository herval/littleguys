// Derived, display-level facts about little guys and their threads, across
// every connected gateway.
import type { AgentSummary, Capabilities, ConnStatus, GatewayRoster, Guy, Roster, SessionRow } from "./types";

export interface GuyActivity {
  running: boolean;
  unread: number;
  needsYou: boolean;
  /** Most recent notable thread (main/dashboard) */
  latest: SessionRow | null;
  status: string;
}

export function guyUid(gatewayId: string, agentId: string): string {
  return `${gatewayId}/${agentId}`;
}

export function splitUid(uid: string): { gatewayId: string; agentId: string } {
  const i = uid.indexOf("/");
  return { gatewayId: uid.slice(0, i), agentId: uid.slice(i + 1) };
}

/** Capabilities of the runtime a guy lives on (OpenClaw's if unknown). */
export function capsOf(roster: Roster, gatewayId: string | null | undefined): Capabilities {
  return gatewayOf(roster, gatewayId)?.status.capabilities ?? FULL_CAPS;
}

export const FULL_CAPS: Capabilities = {
  manageGuys: true,
  routines: true,
  routinesLocal: false,
  approvals: true,
  threadActions: true,
  newThreads: true,
  attachments: true,
  models: true,
  files: false,
  skills: false,
  commands: false,
  tools: false,
  plugins: false,
  browser: false,
  pastSessions: false,
};

/** Is this runtime on this Mac (so its folders can be opened in Finder)? */
export function isOnThisMac(g: GatewayRoster | undefined): boolean {
  if (!g) return false;
  const { mode, url, kind } = g.status;
  if (mode === "local") return true;
  if (url && /\/\/(127\.0\.0\.1|localhost|\[::1\])/.test(url)) return true;
  return kind === "acp" && !(url ?? "").startsWith("ws");
}

export const RUNTIME_LABEL: Record<string, string> = { openclaw: "OpenClaw", hermes: "Hermes", acp: "ACP agent" };

export function liveGateways(roster: Roster): GatewayRoster[] {
  return roster.gateways.filter((g) => g.enabled && g.status.phase === "connected");
}

export function gatewayOf(roster: Roster, id: string | null | undefined): GatewayRoster | undefined {
  return id ? roster.gateways.find((g) => g.id === id) : undefined;
}

/** Every guy on every connected gateway (minus the ones hidden in Settings). */
export function allGuys(roster: Roster, opts: { includeHidden?: boolean } = {}): Guy[] {
  const hidden = new Set(roster.hidden ?? []);
  return liveGateways(roster)
    .flatMap((g) => g.agents.map((a) => ({ ...a, gatewayId: g.id, gatewayName: g.name, uid: guyUid(g.id, a.id) })))
    .filter((guy) => opts.includeHidden || !hidden.has(guy.uid));
}

export function findGuy(roster: Roster, uid: string | null | undefined): Guy | undefined {
  if (!uid) return undefined;
  return allGuys(roster).find((g) => g.uid === uid);
}

/** More than one gateway in play → label guys with where they live. */
export function isMultiGateway(roster: Roster): boolean {
  return roster.gateways.filter((g) => g.enabled).length > 1;
}

export function guyName(agent: Pick<AgentSummary, "id" | "name" | "identity">): string {
  return agent.identity?.name || agent.name || agent.id;
}

export function guyEmoji(agent: Pick<AgentSummary, "identity">): string | null {
  const e = agent.identity?.emoji?.trim();
  return e ? e : null;
}

export function guyAvatarUrl(agent: Pick<AgentSummary, "identity">): string | null {
  const url = agent.identity?.avatarUrl;
  return url && /^(https?:|data:)/.test(url) ? url : null;
}

/** Stable, pleasant pastel per guy (used when there's no avatar image). */
export function guyHue(id: string): number {
  let h = 0;
  for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) >>> 0;
  return h % 360;
}

export type ThreadKind = "main" | "chat" | "channel" | "routine" | "helper";

export function threadKind(key: string): ThreadKind {
  if (/^agent:[^:]+:main$/.test(key)) return "main";
  if (key.includes(":dashboard:")) return "chat";
  if (key.includes(":cron:")) return "routine";
  if (key.includes(":subagent:") || key.includes(":explicit:")) return "helper";
  return "channel";
}

export function channelName(key: string): string | null {
  const parts = key.split(":");
  if (parts.length <= 2) return null;
  // e.g. "ios-d25392d8-3022-…" (OpenClaw's iOS app) → "ios"
  return parts[2].replace(/-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i, "");
}

/**
 * OpenClaw (and Hermes, via its "Bot Chat") give every guy a real main chat.
 * ACP agents only have threads: their `agent:<id>:main` key is just a draft
 * that becomes a thread on the first message.
 */
export function hasMainChat(roster: Roster, gatewayId: string): boolean {
  return gatewayOf(roster, gatewayId)?.status.kind !== "acp";
}

/** Where to land for a guy: their main chat, or (ACP) their latest thread / a new draft. */
export function defaultThread(roster: Roster, guy: Pick<Guy, "gatewayId" | "id">): string {
  const main = `agent:${guy.id}:main`;
  if (hasMainChat(roster, guy.gatewayId)) return main;
  const latest = sessionsFor(roster, guy)
    .filter((r) => !r.archived && ["main", "chat", "channel"].includes(threadKind(r.key)))
    .sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0))[0];
  return latest?.key ?? main;
}

export function threadTitle(
  row: Pick<SessionRow, "key" | "label" | "displayName" | "derivedTitle"> & { updatedAt?: number | null },
  mainChat = true,
): string {
  const kind = threadKind(row.key);
  if (kind === "main" && mainChat) return row.label || "Main chat";
  const title = row.label || row.derivedTitle || row.displayName;
  if (title) return title;
  if (kind === "channel") {
    const ch = channelName(row.key);
    return ch ? `${ch[0].toUpperCase()}${ch.slice(1)} conversation` : "Conversation";
  }
  if (kind === "routine") return "Routine run";
  return row.updatedAt ? "Untitled chat" : "New chat";
}

function agentIdOf(key: string): string | null {
  const p = key.split(":");
  return p[0] === "agent" ? p[1] : null;
}

export function sessionsFor(roster: Roster, guy: Pick<Guy, "gatewayId" | "id">): SessionRow[] {
  const g = gatewayOf(roster, guy.gatewayId);
  return g ? g.sessions.filter((s) => (s.agentId ?? agentIdOf(s.key)) === guy.id) : [];
}

export function sessionRow(roster: Roster, gatewayId: string, key: string): SessionRow | undefined {
  return gatewayOf(roster, gatewayId)?.sessions.find((s) => s.key === key);
}

export function isRunning(roster: Roster, gatewayId: string, key: string): boolean {
  return Boolean(gatewayOf(roster, gatewayId)?.running.includes(key));
}

export function activityFor(roster: Roster, guy: Pick<Guy, "gatewayId" | "id">): GuyActivity {
  const g = gatewayOf(roster, guy.gatewayId);
  const rows = sessionsFor(roster, guy);
  const notable = rows.filter((r) => {
    const k = threadKind(r.key);
    return k === "main" || k === "chat";
  });
  const running = Boolean(g?.running.some((k) => agentIdOf(k) === guy.id && threadKind(k) !== "routine"));
  const unread = notable.filter((r) => r.unread).length;
  const needsYou = Boolean(
    g?.approvals.some(
      (a) => (a.agentId ?? a.request?.agentId ?? agentIdOf(a.sessionKey ?? a.request?.sessionKey ?? "")) === guy.id,
    ),
  );
  const latest = [...notable].sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0))[0] ?? null;
  const status = needsYou
    ? "needs your OK"
    : running
      ? "working…"
      : unread
        ? unread === 1
          ? "new reply"
          : `${unread} new replies`
        : latest?.updatedAt
          ? relativeTime(latest.updatedAt)
          : "say hi";
  return { running, unread, needsYou, latest, status };
}

export function relativeTime(ms: number, now = Date.now()): string {
  const s = Math.round((now - ms) / 1000);
  if (s < 45) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.round(h / 24);
  if (d < 7) return `${d}d ago`;
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function shortTime(ms: number): string {
  const d = new Date(ms);
  const today = new Date();
  if (d.toDateString() === today.toDateString()) {
    return d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  }
  return relativeTime(ms);
}

/** Strip OpenClaw reply directives and system noise from preview text. */
export function cleanPreview(text: string | null | undefined): string {
  if (!text) return "";
  return text
    .replace(/\[\[[a-z_]+\]\]\s*/gi, "")
    .replace(/\s+/g, " ")
    .trim();
}

/**
 * The gateway turned the connection away at the door, before any token could
 * be sent: it's behind a Tailscale Serve route (or proxy) it doesn't trust.
 */
export function needsProxyFix(status: ConnStatus | null | undefined): boolean {
  return status?.phase === "error" && status.detailCode === "PROXY_ATTRIBUTION_REQUIRED";
}

/** `phaseLabel`, but naming errors you can fix. */
export function statusLabel(status: ConnStatus | undefined): string {
  return needsProxyFix(status) ? "Needs a setting" : phaseLabel(status?.phase);
}

/** Needs something from you (approval, a token, a setting), rather than just waiting. */
export function needsAction(status: ConnStatus | undefined): boolean {
  return status?.phase === "pairing" || status?.phase === "auth-failed" || needsProxyFix(status);
}

export function phaseLabel(phase: string | undefined): string {
  switch (phase) {
    case "connected":
      return "Online";
    case "pairing":
      return "Needs approval";
    case "auth-failed":
      return "Needs a token";
    case "offline":
      return "Offline";
    case "no-config":
      return "Not set up";
    case "error":
      return "Error";
    default:
      return "Connecting…";
  }
}
