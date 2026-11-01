import { ChevronRight } from "lucide-react";
import { useMemo } from "react";
import { shortcut } from "../lib/platform";
import { activityFor, cleanPreview, guyName, hasMainChat, isMultiGateway, shortTime, threadKind, threadTitle } from "../lib/guys";
import type { Guy, Roster } from "../lib/types";
import { recentGuys, useApp } from "../stores/app";
import { Avatar } from "../ui/Avatar";
import { TypingDots } from "../ui/kit";

/**
 * Every little guy (from every gateway) as a card, like WhatsApp's chat list:
 * newest message first. Clicking one shows their threads without leaving the open chat.
 */
export function GuyList() {
  const roster = useApp((s) => s.roster);
  const order = useApp((s) => s.order);
  const selectedUid = useApp((s) => s.selectedUid);
  const browse = useApp((s) => s.browse);
  const guys = useMemo(() => recentGuys(roster, order), [roster, order]);
  const multi = isMultiGateway(roster);

  return (
    <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
      {guys.map((guy, i) => (
        <GuyCard
          key={guy.uid}
          roster={roster}
          guy={guy}
          index={i}
          showGateway={multi}
          selected={guy.uid === selectedUid}
          last={i === guys.length - 1}
          onSelect={() => browse(guy.uid)}
        />
      ))}
    </div>
  );
}

function GuyCard({
  roster,
  guy,
  index,
  showGateway,
  selected,
  last,
  onSelect,
}: {
  roster: Roster;
  guy: Guy;
  index: number;
  showGateway: boolean;
  selected: boolean;
  last: boolean;
  onSelect: () => void;
}) {
  const act = activityFor(roster, guy);
  const ring = act.needsYou ? "needs-you" : act.running ? "working" : act.unread ? "unread" : "idle";
  const latest = act.latest;
  const preview = cleanPreview(latest?.lastMessagePreview);
  // Which thread the latest message is in, unless it's the main chat (that's just "the guy").
  const withMain = hasMainChat(roster, guy.gatewayId);
  const thread = latest && !(withMain && threadKind(latest.key) === "main") ? threadTitle(latest, withMain) : null;

  return (
    <button
      onClick={onSelect}
      title={`${guyName(guy)}${showGateway ? ` on ${guy.gatewayName}` : ""}${index < 9 ? ` (${shortcut(String(index + 1))})` : ""}`}
      className={`flex w-full items-center gap-3 rounded-xl pl-2 text-left outline-none transition ${
        selected ? "bg-[var(--color-chip)]" : "hover:bg-[var(--color-chip)]"
      }`}
    >
      <Avatar agent={guy} size={46} ring={ring} />
      <div className={`flex min-w-0 flex-1 items-center gap-2 py-3 pr-2 ${last || selected ? "" : "border-b border-[var(--color-line)]"}`}>
        <div className="min-w-0 flex-1">
          <div className="flex items-baseline gap-2">
            <span className="truncate font-display text-[14px] font-semibold">{guyName(guy)}</span>
            {showGateway && guy.gatewayName !== guyName(guy) && (
              <span className="shrink-0 truncate text-[11px] text-[var(--color-ink-3)]">{guy.gatewayName}</span>
            )}
            <span
              className={`ml-auto shrink-0 text-[11px] ${
                act.needsYou
                  ? "font-medium text-[var(--color-bad)]"
                  : act.unread
                    ? "font-medium text-[var(--color-accent-strong)]"
                    : "text-[var(--color-ink-3)]"
              }`}
            >
              {act.needsYou ? "needs your OK" : act.running ? <TypingDots /> : latest?.updatedAt ? shortTime(latest.updatedAt) : ""}
            </span>
          </div>
          <div className="mt-0.5 flex items-center gap-2">
            <p className={`min-w-0 flex-1 truncate text-[12.5px] ${act.unread ? "text-[var(--color-ink)]" : "text-[var(--color-ink-3)]"}`}>
              {thread && (
                <span className="text-[var(--color-ink-2)]">
                  {thread} <span className="text-[10px]">▸</span>{" "}
                </span>
              )}
              {act.running ? "working…" : preview || "Say hi"}
            </p>
            {act.unread > 0 && (
              <span className="min-w-[18px] shrink-0 rounded-full bg-[var(--color-accent)] px-1.5 text-center text-[10.5px] font-bold leading-[18px] text-white">
                {act.unread}
              </span>
            )}
          </div>
        </div>
        <ChevronRight size={15} className="shrink-0 text-[var(--color-ink-3)]/70" />
      </div>
    </button>
  );
}
