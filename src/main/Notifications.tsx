import { type ReactNode, useMemo, useState } from "react";
import { allGuys, guyName, isMultiGateway } from "../lib/guys";
import { core, errorMessage } from "../lib/ipc";
import type { NotifyPrefs, RoutineNotifications } from "../lib/types";
import { useApp } from "../stores/app";
import { Avatar } from "../ui/Avatar";
import { Segmented, Switch } from "../ui/kit";

const ROUTINES: Array<{ value: RoutineNotifications; label: string }> = [
  { value: "all", label: "All" },
  { value: "failing", label: "Failures" },
  { value: "off", label: "Off" },
];

/** Settings section: what notifies you, for every guy at once and per guy. Approvals always do. */
export function Notifications() {
  const roster = useApp((s) => s.roster);
  const guys = useMemo(() => allGuys(roster), [roster]);
  const multi = isMultiGateway(roster);
  const { everyone, guys: setApart } = roster.notifications;
  const [error, setError] = useState<string | null>(null);

  if (guys.length === 0) return null;

  const save = (uid: string | null, prefs: NotifyPrefs) => {
    setError(null);
    core.setNotifications(uid, prefs).catch((e) => setError(errorMessage(e)));
  };

  return (
    <section className="mt-5">
      <div className="mb-1.5 flex items-baseline gap-3 px-3 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">
        <h3 className="-ml-3 flex-1">Notifications</h3>
        <span className="w-[60px] text-center">Replies</span>
        <span className="w-[196px] text-center">Routines</span>
      </div>
      <div className="overflow-hidden rounded-xl border border-[var(--color-line)]">
        <PrefsRow prefs={everyone} onChange={(prefs) => save(null, prefs)} className="bg-[var(--color-chip)]">
          <p className="py-1 text-[12.5px] font-semibold">Everyone</p>
        </PrefsRow>
        {guys.map((guy) => (
          <PrefsRow
            key={guy.uid}
            prefs={setApart[guy.uid] ?? everyone}
            onChange={(prefs) => save(guy.uid, prefs)}
            className="border-t border-[var(--color-line)]"
          >
            <Avatar agent={guy} size={26} />
            <div className="min-w-0">
              <p className="truncate text-[12.5px] font-medium">{guyName(guy)}</p>
              {multi && <p className="truncate text-[11px] text-[var(--color-ink-3)]">{guy.gatewayName}</p>}
            </div>
          </PrefsRow>
        ))}
      </div>
      <p className="mt-1.5 text-[11px] text-[var(--color-ink-3)]">
        Everyone sets every guy at once, and new guys start there. Approvals always notify.
      </p>
      {error && <p className="mt-1 text-[11.5px] text-[var(--color-bad)]">{error}</p>}
    </section>
  );
}

function PrefsRow({
  prefs,
  onChange,
  className,
  children,
}: {
  prefs: NotifyPrefs;
  onChange: (prefs: NotifyPrefs) => void;
  className: string;
  children: ReactNode;
}) {
  return (
    <div className={`flex items-center gap-3 px-3 py-2 ${className}`}>
      <div className="flex min-w-0 flex-1 items-center gap-2.5">{children}</div>
      <div className="flex w-[60px] justify-center">
        <Switch
          on={prefs.replies}
          label={prefs.replies ? "Replies notify you" : "Replies don't notify you"}
          onChange={(replies) => onChange({ ...prefs, replies })}
        />
      </div>
      <div className="w-[196px]">
        <Segmented value={prefs.routines} options={ROUTINES} onChange={(routines) => onChange({ ...prefs, routines })} />
      </div>
    </div>
  );
}
