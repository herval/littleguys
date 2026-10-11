import { AlertCircle, ChevronRight } from "lucide-react";
import { needsAction, statusLabel } from "../lib/guys";
import { useApp } from "../stores/app";

/**
 * When some gateways work and others need attention (approval, token,
 * offline), say so in one quiet line instead of blocking the app.
 */
export function GatewayBanner() {
  const roster = useApp((s) => s.roster);
  const setSettingsOpen = useApp((s) => s.setSettingsOpen);
  const troubled = roster.gateways.filter(
    (g) => g.enabled && ["pairing", "auth-failed", "offline", "error"].includes(g.status.phase),
  );
  if (troubled.length === 0) return null;
  const urgent = troubled.some((g) => needsAction(g.status));
  return (
    <button
      onClick={() => setSettingsOpen(true)}
      className={`mx-3 mb-2 flex items-center gap-2 rounded-lg px-3 py-1.5 text-left text-[12px] transition ${
        urgent
          ? "bg-[var(--color-warn)]/12 text-[var(--color-ink)] hover:bg-[var(--color-warn)]/18"
          : "bg-[var(--color-chip)] text-[var(--color-ink-2)] hover:bg-black/8 dark:hover:bg-white/10"
      }`}
    >
      <AlertCircle size={13} className={urgent ? "text-[var(--color-warn)]" : "opacity-60"} />
      <span className="min-w-0 flex-1 truncate">
        {troubled.map((g, i) => (
          <span key={g.id}>
            {i > 0 && " · "}
            <span className="font-medium">{g.name}</span> {statusLabel(g.status).toLowerCase()}
          </span>
        ))}
      </span>
      <span className="flex shrink-0 items-center text-[11.5px] opacity-70">
        {urgent ? "Fix" : "Details"} <ChevronRight size={12} />
      </span>
    </button>
  );
}
