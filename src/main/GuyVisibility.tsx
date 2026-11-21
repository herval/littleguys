import { Eye, EyeOff, Plus } from "lucide-react";
import { useMemo, useState } from "react";
import { TRAY } from "../lib/platform";
import { allGuys, guyName, isMultiGateway } from "../lib/guys";
import { core, errorMessage } from "../lib/ipc";
import type { Guy } from "../lib/types";
import { useApp } from "../stores/app";
import { Avatar } from "../ui/Avatar";
import { Button, Switch } from "../ui/kit";

/** Settings section: choose which little guys show up in littleguys, and whose steps their threads show. */
export function GuyVisibility() {
  const roster = useApp((s) => s.roster);
  const guys = useMemo(() => allGuys(roster, { includeHidden: true }), [roster]);
  const hidden = useMemo(() => new Set(roster.hidden ?? []), [roster.hidden]);
  const steps = useMemo(() => new Set(roster.steps ?? []), [roster.steps]);
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

  const toggleSteps = (guy: Guy, shown: boolean) => {
    setError(null);
    core.setGuySteps(guy.uid, shown).catch((e) => setError(errorMessage(e)));
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
      <div className="mb-1 flex gap-3 px-3 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">
        <span className="flex-1" />
        <span className="w-[44px] text-center">Steps</span>
        <span className="w-[76px]" />
      </div>
      <div className="overflow-hidden rounded-xl border border-[var(--color-line)]">
        {guys.map((guy, i) => {
          const isHidden = hidden.has(guy.uid);
          const showsSteps = steps.has(guy.uid);
          return (
            <div key={guy.uid} className={`flex items-center gap-3 px-3 py-2 ${i ? "border-t border-[var(--color-line)]" : ""}`}>
              <div className={`flex min-w-0 flex-1 items-center gap-2.5 ${isHidden ? "opacity-55" : ""}`}>
                <Avatar agent={guy} size={26} />
                <div className="min-w-0">
                  <p className="truncate text-[12.5px] font-medium">{guyName(guy)}</p>
                  {multi && <p className="truncate text-[11px] text-[var(--color-ink-3)]">{guy.gatewayName}</p>}
                </div>
              </div>
              <div className="flex w-[44px] justify-center">
                <Switch
                  on={showsSteps}
                  label={showsSteps ? `${guyName(guy)}'s threads show their steps` : `${guyName(guy)}'s threads show only messages`}
                  onChange={(on) => toggleSteps(guy, on)}
                />
              </div>
              <button
                role="switch"
                aria-checked={!isHidden}
                onClick={() => toggle(guy)}
                title={isHidden ? `Show ${guyName(guy)}` : `Hide ${guyName(guy)}`}
                className={`flex w-[76px] items-center gap-1 rounded-md px-2 py-1 text-[11.5px] transition ${
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
        Steps are the “Ran 3 commands…” lines between a guy's messages. Off, their threads show just what they say, and in coder mode the
        edits they make.
      </p>
      <p className="mt-1 text-[11px] text-[var(--color-ink-3)]">
        Hidden guys stay on their gateway; they just don't appear in the {TRAY} or the sidebar, and won't notify you — except when they
        need your OK.
      </p>
      {error && <p className="mt-1 text-[11.5px] text-[var(--color-bad)]">{error}</p>}
    </section>
  );
}
