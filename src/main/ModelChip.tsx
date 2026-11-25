// The model and effort a thread runs on, in the composer (Claude Code, Codex: `threadModels`).
// A new thread starts on what you pick here; a started one switches from its next turn
// (`switchModels`), or keeps the model it started on.
import { Check, ChevronDown } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { sessions as sessionsApi } from "../lib/gateway";
import { capsOf, guyName } from "../lib/guys";
import { core } from "../lib/ipc";
import { chipLabel, effortShort, effortsFor, loadThreadModels, modelName, shown } from "../lib/models";
import type { Guy, ModelPick, RowModel, ThreadModels } from "../lib/types";
import { reportError } from "../ui/reportError";
import { useApp } from "../stores/app";

/** What you picked for a thread that hasn't started, until its first message (kept per thread for this window). */
export function usePendingPick(threadId: string): [ModelPick, (pick: ModelPick) => void] {
  const storageKey = `model:${threadId}`;
  const read = (): ModelPick => {
    try {
      return JSON.parse(sessionStorage.getItem(storageKey) ?? "{}") as ModelPick;
    } catch {
      return {};
    }
  };
  const [pick, setPick] = useState<ModelPick>(read);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(() => setPick(read()), [storageKey]);
  const set = (next: ModelPick) => {
    if (next.model || next.effort) sessionStorage.setItem(storageKey, JSON.stringify(next));
    else sessionStorage.removeItem(storageKey);
    setPick(next);
  };
  return [pick, set];
}

export function ModelChip({
  guy,
  sessionKey,
  row,
  unstarted,
  pick,
  onPick,
}: {
  guy: Guy;
  sessionKey: string;
  /** The thread's session row: what it last ran on, and what you picked. */
  row?: RowModel;
  unstarted: boolean;
  /** A new thread's pick (see [`usePendingPick`]). */
  pick: ModelPick;
  onPick: (pick: ModelPick) => void;
}) {
  const gw = guy.gatewayId;
  const caps = useApp((s) => capsOf(s.roster, gw));
  const [list, setList] = useState<ThreadModels | null>(null);
  const [open, setOpen] = useState(false);
  // A started thread's switch, until its row says so.
  const [sent, setSent] = useState<ModelPick | null>(null);
  const ref = useRef<HTMLDivElement>(null);

  const enabled = Boolean(caps.threadModels);
  const editable = enabled && (unstarted || Boolean(caps.switchModels));

  useEffect(() => {
    if (!enabled) return;
    let live = true;
    loadThreadModels(gw, guy.id).then((l) => live && setList(l));
    return () => {
      live = false;
    };
  }, [enabled, gw, guy.id]);

  const rowModelChoice = row?.modelChoice ?? null;
  const rowEffortChoice = row?.effortChoice ?? null;
  useEffect(() => setSent(null), [rowModelChoice, rowEffortChoice, sessionKey]);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => !ref.current?.contains(e.target as Node) && setOpen(false);
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);

  if (!enabled) return null;
  const picked: ModelPick = unstarted ? pick : (sent ?? { model: rowModelChoice, effort: rowEffortChoice });
  const now = shown(picked, unstarted ? undefined : row, list);
  // A started thread that can't switch shows what it runs on, once its transcript says.
  if (!editable && !now.model) return null;
  const label = chipLabel(now, list);
  const efforts = effortsFor(list, now.model);

  const choose = (next: ModelPick) => {
    if (unstarted) {
      onPick(next);
      return;
    }
    setSent(next);
    sessionsApi.patch(gw, sessionKey, { model: next.model ?? null, effort: next.effort ?? null }).then(
      () => core.refreshRoster(gw),
      (e) => {
        setSent(null);
        void reportError(e, "Couldn't switch the model");
      },
    );
  };
  const pickModel = (model: string | null) => {
    // An effort the new model doesn't take goes back to its default.
    const keep = now.picked.effort && effortsFor(list, model ?? list?.default?.model).includes(now.picked.effort);
    choose({ model, effort: keep ? now.picked.effort : null });
  };
  const defaultName = list?.default?.model ? modelName(list.default.model, list.models) : null;

  if (!editable) {
    return (
      <span
        title="This thread keeps the model it started on. Pick another for a new thread."
        className="mb-[calc(18px_-_1rem)] flex h-8 shrink-0 items-center px-2.5 text-[12px] font-medium text-[var(--color-ink-3)]"
      >
        {label}
      </span>
    );
  }

  return (
    <div ref={ref} className="relative mb-[calc(18px_-_1rem)] shrink-0" data-model-chip>
      <button
        onClick={() => setOpen((o) => !o)}
        title="Model and effort"
        aria-haspopup="dialog"
        aria-expanded={open}
        className={`flex h-8 items-center gap-1 rounded-full px-2.5 text-[12px] font-medium transition hover:bg-[var(--color-chip)] hover:text-[var(--color-ink)] ${open ? "bg-[var(--color-chip)] text-[var(--color-ink)]" : "text-[var(--color-ink-2)]"}`}
      >
        {label}
        <ChevronDown size={13} className="text-[var(--color-ink-3)]" />
      </button>
      {open && (
        <div
          role="dialog"
          aria-label="Model and effort"
          className="absolute bottom-full right-0 z-30 mb-2 w-[320px] animate-fade overflow-hidden rounded-xl border border-[var(--color-line)] bg-[var(--color-pane-solid)] shadow-[0_6px_24px_rgba(0,0,0,0.12)]"
        >
          <p className="px-3 pb-1 pt-2.5 text-[10.5px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">Model</p>
          <div className="max-h-[300px] overflow-y-auto px-1">
            <ModelRow
              name="Default"
              detail={defaultName ? `${defaultName}, from ${guyName(guy)}’s settings` : `What ${guyName(guy)} uses unless you pick`}
              on={!now.picked.model}
              onClick={() => pickModel(null)}
            />
            {list?.models.map((m) => (
              <ModelRow key={m.id} name={m.name ?? modelName(m.id)} detail={m.description ?? undefined} on={now.picked.model === m.id} onClick={() => pickModel(m.id)} />
            ))}
            {!list && <p className="px-2.5 py-2 text-[12px] text-[var(--color-ink-3)]">Loading…</p>}
          </div>
          {efforts.length > 0 && (
            <>
              <p className="px-3 pb-1.5 pt-3 text-[10.5px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">Effort</p>
              <div className="mx-3 mb-3 flex rounded-lg bg-[var(--color-chip)] p-0.5" role="radiogroup" aria-label="Effort">
                {efforts.map((e) => (
                  <button
                    key={e}
                    role="radio"
                    aria-checked={e === now.effort}
                    onClick={() => choose({ model: now.picked.model, effort: e })}
                    className={`flex-1 rounded-md py-1 text-[12px] transition ${e === now.effort ? "bg-[var(--color-pane-solid)] font-medium text-[var(--color-ink)] shadow-sm" : "text-[var(--color-ink-3)] hover:text-[var(--color-ink)]"}`}
                  >
                    {effortShort(e)}
                  </button>
                ))}
              </div>
            </>
          )}
          <p className="border-t border-[var(--color-line)] px-3 py-1.5 text-[11px] text-[var(--color-ink-3)]">
            {unstarted ? "This thread starts on it." : "From this thread's next turn."}
          </p>
        </div>
      )}
    </div>
  );
}

function ModelRow({ name, detail, on, onClick }: { name: string; detail?: string; on: boolean; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      role="menuitemradio"
      aria-checked={on}
      className={`flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left ${on ? "bg-[var(--color-chip)]" : "hover:bg-[var(--color-chip)]"}`}
    >
      <span className="min-w-0 flex-1">
        <span className="block text-[13px] text-[var(--color-ink)]">{name}</span>
        {detail && <span className="block truncate text-[11.5px] text-[var(--color-ink-3)]">{detail}</span>}
      </span>
      {on && <Check size={14} className="shrink-0 text-[var(--color-accent)]" />}
    </button>
  );
}
