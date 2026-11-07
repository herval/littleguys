// The usage-limit line on a guy's row in the popover. The numbers come from
// src-tauri/src/limits.rs, keyed by provider.
import { isOnThisMac, relativeTime } from "./guys";
import type { GatewayRoster, Limit, LimitProvider } from "./types";

/** A reading older than this says how old it is, so it doesn't pass for current. */
const FRESH_MS = 60 * 60_000;
const NEARLY_OUT = 80;
const OUT = 95;

/** A Codex guy added as an ACP agent, on this computer. ACP agents show their command as the url. */
export function isAcpCodexHere(gateway: GatewayRoster): boolean {
  const { kind, url } = gateway.status;
  return kind === "acp" && isOnThisMac(gateway) && /codex/i.test(url ?? "");
}

/** Whose rate limit a gateway's guys run on, if littleguys can tell. */
export function limitProvider(gateway: GatewayRoster | undefined): LimitProvider | null {
  if (!gateway) return null;
  const { kind } = gateway.status;
  if (kind === "claudecode") return "claude";
  if (kind === "codex") return "codex";
  // On another machine, its limit is that machine's.
  if (isAcpCodexHere(gateway)) return "codex";
  return null;
}

function timeLeft(ms: number): string {
  const minutes = Math.ceil(ms / 60_000);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  return hours < 48 ? `${hours}h` : `${Math.round(hours / 24)}d`;
}

export interface LimitLine {
  percent: number;
  /** "41% · resets in 2h" */
  text: string;
  tone: "quiet" | "warn" | "bad";
}

/** What to show for a reading, or null once its window is over. */
export function limitLine(limit: Limit | null | undefined, now = Date.now()): LimitLine | null {
  if (!limit || limit.resetsAtMs <= now) return null;
  const percent = Math.round(limit.usedPercent);
  const age = now - limit.readAtMs > FRESH_MS ? ` · as of ${relativeTime(limit.readAtMs, now)}` : "";
  return {
    percent,
    text: `${percent}% · resets in ${timeLeft(limit.resetsAtMs - now)}${age}`,
    tone: percent >= OUT ? "bad" : percent >= NEARLY_OUT ? "warn" : "quiet",
  };
}
