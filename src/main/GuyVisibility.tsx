import { Eye, EyeOff, Plus } from "lucide-react";
import { useMemo, useState } from "react";
import { TRAY } from "../lib/platform";
import { allGuys, guyName, isMultiGateway } from "../lib/guys";
import { core, errorMessage } from "../lib/ipc";
import type { Guy } from "../lib/types";
import { useApp } from "../stores/app";
import { Avatar } from "../ui/Avatar";
import { Button } from "../ui/kit";

/** Settings section: choose which little guys show up in littleguys. */
export function GuyVisibility() {
  const roster = useApp((s) => s.roster);
  const guys = useMemo(() => allGuys(roster, { includeHidden: true }), [roster]);
  const hidden = useMemo(() => new Set(roster.hidden ?? []), [roster.hidden]);
  const multi = isMultiGateway(roster);
  const [error, setError] = useState<string | null>(null);
  // A gateway that makes guys (OpenClaw); without one, Connections has "Add a bot".
  const canMake = useApp((s) => s.roster.gateways.some((g) => g.loaded && g.status.capabilities?.manageGuys));
  const newGuy = () => {
    useApp.getState().setSettingsOpen(false);
    useApp.getState().startNewGuy();
  };
  const newButton = canMake && (
    <Button size="sm" onClick={newGuy}>
      <Plus size={12} /> New little guy
    </Button>
  );

  if (guys.length === 0) return newButton ? <section className="mt-5 flex justify-end">{newButton}</section> : null;
  const hiddenCount = guys.filter((g) => hidden.has(g.uid)).length;

  const toggle = async (guy: Guy) => {
    setError(null);
    try {
      await core.setGuyHidden(guy.uid, !hidden.has(guy.uid));
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  return (
    <section className="mt-5">
      <div className="mb-1.5 flex items-center justify-between gap-3">
        <h3 className="text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">
          littleguys
          <span className="ml-2 font-normal normal-case tracking-normal">
            {guys.length - hiddenCount} shown{hiddenCount ? ` · ${hiddenCount} hidden` : ""}
          </span>
        </h3>
        {newButton}
      </div>
      <div className="overflow-hidden rounded-xl border border-[var(--color-line)]">
        {guys.map((guy, i) => {
          const isHidden = hidden.has(guy.uid);
          return (
            <div
              key={guy.uid}
              className={`flex items-center gap-2.5 px-3 py-2 ${i ? "border-t border-[var(--color-line)]" : ""} ${isHidden ? "opacity-55" : ""}`}
            >
              <Avatar agent={guy} size={26} />
              <div className="min-w-0 flex-1">
                <p className="truncate text-[12.5px] font-medium">{guyName(guy)}</p>
                {multi && <p className="truncate text-[11px] text-[var(--color-ink-3)]">{guy.gatewayName}</p>}
              </div>
              <button
                role="switch"
                aria-checked={!isHidden}
                onClick={() => toggle(guy)}
                title={isHidden ? `Show ${guyName(guy)}` : `Hide ${guyName(guy)}`}
                className={`flex items-center gap-1 rounded-md px-2 py-1 text-[11.5px] transition ${
                  isHidden
                    ? "text-[var(--color-ink-3)] hover:bg-[var(--color-chip)] hover:text-[var(--color-ink)]"
                    : "text-[var(--color-ink-2)] hover:bg-[var(--color-chip)]"
                }`}
              >
                {isHidden ? <EyeOff size={13} /> : <Eye size={13} />}
                {isHidden ? "Hidden" : "Shown"}
              </button>
            </div>
          );
        })}
      </div>
      <p className="mt-1.5 text-[11px] text-[var(--color-ink-3)]">
        Hidden guys stay on their gateway; they just don't appear in the {TRAY} or the stories row, and won't notify you — except when they
        need your OK.
      </p>
      {error && <p className="mt-1 text-[11.5px] text-[var(--color-bad)]">{error}</p>}
    </section>
  );
}
