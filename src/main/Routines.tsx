import { CalendarClock, ChevronRight, Pencil, Play, Plus, Trash2 } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { cron } from "../lib/gateway";
import { capsOf, gatewayOf, guyName, hasMainChat, relativeTime, sessionsFor, threadKind, threadTitle } from "../lib/guys";
import { errorMessage, listen } from "../lib/ipc";
import {
  DAYS,
  describeNext,
  describeSchedule,
  localTimeZone,
  presetToSchedule,
  scheduleToPreset,
  type CronSchedule,
  type Preset,
} from "../lib/schedule";
import type { CronJob, CronRunLogEntry, Guy } from "../lib/types";
import { mainSessionKey, useApp } from "../stores/app";
import { GuyMark } from "../ui/Avatar";
import { confirmAction } from "../ui/confirm";
import { Button, Field, IconButton, inputBase, inputClass, Modal, Spinner, Switch } from "../ui/kit";
import { FolderPicker } from "./FolderPicker";

type Job = CronJob & {
  lastRunStatus?: string | null;
  lastRunAtMs?: number | null;
  nextRunAtMs?: number | null;
  declarationKey?: string;
  displayName?: string;
};

export function RoutinesPanel({ guy }: { guy: Guy }) {
  const agent = guy;
  const agentId = guy.id;
  const gw = guy.gatewayId;
  const runsLocally = useApp((s) => capsOf(s.roster, gw).routinesLocal);
  const [jobs, setJobs] = useState<Job[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Job | "new" | null>(null);
  const [showBuiltIn, setShowBuiltIn] = useState(false);

  const load = useCallback(async () => {
    try {
      const res = await cron.list(gw, agentId);
      setJobs(res.jobs as Job[]);
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, [gw, agentId]);

  useEffect(() => {
    setJobs(null);
    load();
    const unlisten = listen<{ gateway: string; event: string }>("gw:event", (frame) => {
      if (frame.gateway === gw && frame.event === "cron") load();
    });
    return () => {
      unlisten.then((u) => u());
    };
  }, [load, gw]);

  const mine = useMemo(() => (jobs ?? []).filter((j) => !j.declarationKey), [jobs]);
  const builtIn = useMemo(() => (jobs ?? []).filter((j) => j.declarationKey), [jobs]);
  return (
    <div className="flex h-full min-h-0 flex-col">
      <div data-tauri-drag-region className="flex h-12 shrink-0 items-center justify-between border-b border-[var(--color-line)] px-5">
        <div data-tauri-drag-region>
          <h3 className="text-[13.5px] font-semibold">Routines</h3>
          <p className="text-[11px] text-[var(--color-ink-3)]">
            Things {guyName(agent)} does on a schedule
            {runsLocally ? " · run by littleguys on this Mac, while it's open" : ""}
          </p>
        </div>
        <Button variant="primary" size="sm" onClick={() => setEditing("new")}>
          <Plus size={13} /> New routine
        </Button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
        <div className="mx-auto max-w-[760px]">
          {error && <p className="mb-3 text-[12.5px] text-[var(--color-bad)]">{error}</p>}
          {jobs === null && !error && (
            <div className="flex justify-center py-10">
              <Spinner size={18} />
            </div>
          )}
          {jobs && mine.length === 0 && (
            <div className="flex flex-col items-center py-12 text-center">
              <GuyMark size={80} />
              <p className="mt-2 font-display text-[15px] font-semibold">No routines yet</p>
              <p className="mt-1 max-w-sm text-[12.5px] text-[var(--color-ink-3)]">
                Give {guyName(agent)} a recurring job — a morning briefing, a weekly inbox sweep, checking prices every few hours.
              </p>
              <div className="mt-4 flex flex-wrap justify-center gap-2">
                {TEMPLATES.map((t) => (
                  <button
                    key={t.name}
                    onClick={() => setEditing({ ...(t as unknown as Job), id: "" })}
                    className="rounded-full border border-[var(--color-line)] px-3 py-1.5 text-[12px] transition hover:border-[var(--color-accent)] hover:text-[var(--color-accent-strong)]"
                  >
                    {t.emoji} {t.name}
                  </button>
                ))}
              </div>
            </div>
          )}
          <div className="space-y-2.5">
            {mine.map((job) => (
              <RoutineCard key={job.id} gw={gw} job={job} onEdit={() => setEditing(job)} onChanged={load} />
            ))}
          </div>
          {builtIn.length > 0 && (
            <div className="mt-6">
              <button
                onClick={() => setShowBuiltIn(!showBuiltIn)}
                className="flex items-center gap-1 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)] hover:text-[var(--color-ink-2)]"
              >
                <ChevronRight size={12} className={`transition ${showBuiltIn ? "rotate-90" : ""}`} />
                Built-in upkeep <span className="font-normal normal-case tracking-normal">{builtIn.length}</span>
              </button>
              {showBuiltIn && (
                <div className="mt-2 space-y-2 opacity-80">
                  {builtIn.map((job) => (
                    <RoutineCard key={job.id} gw={gw} job={job} builtIn onEdit={() => {}} onChanged={load} />
                  ))}
                </div>
              )}
            </div>
          )}
        </div>
      </div>
      {editing && (
        <RoutineEditor
          guy={guy}
          job={editing === "new" ? null : editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            load();
          }}
        />
      )}
    </div>
  );
}

const TEMPLATES = [
  { emoji: "☀️", name: "Morning briefing", schedule: { kind: "cron", expr: "0 8 * * 1-5" }, payload: { kind: "agentTurn", message: "Give me a short morning briefing: weather where I am, my calendar for today if you can see it, and anything I asked you to keep an eye on." } },
  { emoji: "🧹", name: "Weekly tidy-up", schedule: { kind: "cron", expr: "0 17 * * 5" }, payload: { kind: "agentTurn", message: "Review your workspace and notes from this week. Summarize what we got done and what's still open." } },
  { emoji: "👀", name: "Keep an eye on…", schedule: { kind: "every", everyMs: 4 * 3_600_000 }, payload: { kind: "agentTurn", message: "Check <website or thing> and tell me only if something changed." } },
];

function statusBadge(status?: string | null) {
  if (!status) return null;
  const ok = status === "ok" || status === "succeeded";
  return (
    <span
      className={`rounded-full px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide ${
        ok ? "bg-[var(--color-ok)]/12 text-[var(--color-ok)]" : "bg-[var(--color-bad)]/12 text-[var(--color-bad)]"
      }`}
    >
      {ok ? "ok" : status}
    </span>
  );
}

function RoutineCard({
  gw,
  job,
  builtIn,
  onEdit,
  onChanged,
}: {
  gw: string;
  job: Job;
  builtIn?: boolean;
  onEdit: () => void;
  onChanged: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const selectSession = useApp((s) => s.selectSession);

  const act = async (label: string, fn: () => Promise<unknown>) => {
    setBusy(label);
    setErr(null);
    try {
      await fn();
      onChanged();
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  const message = job.payload && "message" in job.payload ? String(job.payload.message) : "";
  const enabled = job.enabled !== false;

  return (
    <div className="rounded-xl border border-[var(--color-line)] bg-[var(--color-pane-solid)] shadow-[0_1px_2px_rgba(0,0,0,0.03)]">
      <div className="flex items-start gap-3 p-3.5">
        <div className={`mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg ${enabled ? "bg-[var(--color-accent-soft)] text-[var(--color-accent-strong)]" : "bg-[var(--color-chip)] text-[var(--color-ink-3)]"}`}>
          <CalendarClock size={16} />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className={`truncate font-medium ${enabled ? "" : "text-[var(--color-ink-3)]"}`}>{job.displayName ?? job.name}</span>
            {statusBadge(job.lastRunStatus)}
          </div>
          <p className="text-[12px] text-[var(--color-ink-2)]">
            {describeSchedule(job.schedule as CronSchedule)}
            {enabled && job.nextRunAtMs ? <span className="text-[var(--color-ink-3)]"> · next {describeNext(job.nextRunAtMs)}</span> : null}
            {!enabled && <span className="text-[var(--color-ink-3)]"> · paused</span>}
          </p>
          {message && <p className="mt-1 line-clamp-2 text-[12px] text-[var(--color-ink-3)]">{message}</p>}
          {err && <p className="mt-1 text-[12px] text-[var(--color-bad)]">{err}</p>}
        </div>
        <div className="flex shrink-0 items-center gap-0.5">
          <Switch
            on={enabled}
            label={enabled ? "Pause" : "Resume"}
            className="mr-1"
            disabled={busy !== null}
            onChange={(on) => act("toggle", () => cron.update(gw, job.id, { enabled: on }))}
          />
          <IconButton label="Run now" disabled={busy !== null} onClick={() => act("run", () => cron.run(gw, job.id))}>
            {busy === "run" ? <Spinner size={12} /> : <Play size={13} />}
          </IconButton>
          {!builtIn && (
            <>
              <IconButton label="Edit" onClick={onEdit}>
                <Pencil size={13} />
              </IconButton>
              <IconButton
                label="Delete"
                disabled={busy !== null}
                onClick={async () => {
                  if (await confirmAction(`Delete the routine “${job.name}”?`)) act("delete", () => cron.remove(gw, job.id));
                }}
              >
                <Trash2 size={13} />
              </IconButton>
            </>
          )}
          <IconButton label="History" onClick={() => setOpen(!open)}>
            <ChevronRight size={14} className={`transition ${open ? "rotate-90" : ""}`} />
          </IconButton>
        </div>
      </div>
      {open && <RunHistory gw={gw} jobId={job.id} onOpen={(key) => selectSession(key)} />}
    </div>
  );
}

function RunHistory({ gw, jobId, onOpen }: { gw: string; jobId: string; onOpen: (sessionKey: string) => void }) {
  const [runs, setRuns] = useState<CronRunLogEntry[] | null>(null);
  useEffect(() => {
    cron
      .runs(gw, jobId, 15)
      .then((r) => setRuns(r.entries))
      .catch(() => setRuns([]));
  }, [gw, jobId]);
  return (
    <div className="border-t border-[var(--color-line)] px-3.5 py-2.5">
      {runs === null ? (
        <Spinner />
      ) : runs.length === 0 ? (
        <p className="text-[12px] text-[var(--color-ink-3)]">Hasn't run yet.</p>
      ) : (
        <ul className="space-y-1">
          {runs.map((r, i) => (
            <li key={i}>
              <button
                disabled={!r.sessionKey}
                onClick={() => r.sessionKey && onOpen(r.sessionKey)}
                className="flex w-full items-center gap-2 rounded-md px-1.5 py-1 text-left text-[12px] hover:bg-[var(--color-chip)] disabled:hover:bg-transparent"
              >
                {statusBadge(r.status)}
                <span className="shrink-0 text-[var(--color-ink-3)]">{relativeTime(r.runAtMs ?? r.ts)}</span>
                <span className="truncate text-[var(--color-ink-2)]">{r.summary ?? r.error ?? ""}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// ---- editor -----------------------------------------------------------------------

function RoutineEditor({ guy, job, onClose, onSaved }: { guy: Guy; job: Job | null; onClose: () => void; onSaved: () => void }) {
  const agentId = guy.id;
  const gw = guy.gatewayId;
  const roster = useApp((s) => s.roster);
  const isNew = !job?.id;
  const [name, setName] = useState(job?.name ?? "");
  const [message, setMessage] = useState(job?.payload && "message" in job.payload ? String(job.payload.message) : "");
  const [preset, setPreset] = useState<Preset>(scheduleToPreset(job?.schedule as CronSchedule | undefined));
  const initialTarget = typeof job?.sessionTarget === "string" && job.sessionTarget.startsWith("session:") ? job.sessionTarget.slice(8) : "";
  const [target, setTarget] = useState<string>(initialTarget); // "" = new thread per run
  // Claude Code starts each run's session in a folder (`payload.cwd`); the guy's is the one you last worked in.
  const startsInFolder = useApp((s) => gatewayOf(s.roster, gw)?.status.kind === "claudecode");
  const [folder, setFolder] = useState<string | null>((job?.payload as { cwd?: string } | undefined)?.cwd ?? guy.workspace ?? null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const threads = useMemo(() => {
    const rows = sessionsFor(roster, guy).filter((r) => ["main", "chat"].includes(threadKind(r.key)));
    const main = mainSessionKey(agentId);
    // ACP agents have no main chat; only their real threads can receive runs.
    if (!hasMainChat(roster, guy.gatewayId)) return rows.map((r) => ({ key: r.key, title: threadTitle(r, false) }));
    return [{ key: main, title: "Main chat" }, ...rows.filter((r) => r.key !== main).map((r) => ({ key: r.key, title: threadTitle(r) }))];
  }, [roster, guy, agentId]);

  const save = async () => {
    if (!name.trim() || !message.trim()) {
      setError("Give the routine a name and tell your guy what to do.");
      return;
    }
    setSaving(true);
    setError(null);
    const fields = {
      name: name.trim(),
      schedule: presetToSchedule(preset),
      sessionTarget: target ? `session:${target}` : "isolated",
      payload: { kind: "agentTurn", message: message.trim(), ...(startsInFolder && !target && folder ? { cwd: folder } : {}) },
      delivery: { mode: "none" },
    };
    try {
      if (isNew) await cron.add(gw, { ...fields, agentId, wakeMode: "now", enabled: true });
      else await cron.update(gw, job!.id, fields);
      onSaved();
    } catch (e) {
      setError(errorMessage(e));
      setSaving(false);
    }
  };

  return (
    <Modal onClose={onClose} width={520}>
      <div className="p-5">
        <h2 className="font-display text-[16px] font-semibold">{isNew ? "New routine" : "Edit routine"}</h2>
        <div className="mt-4 space-y-4">
          <Field label="Name">
            <input className={inputClass} value={name} onChange={(e) => setName(e.target.value)} placeholder="Morning briefing" autoFocus />
          </Field>
          <Field label="What should they do?">
            <textarea
              className={`${inputClass} min-h-[90px] resize-y`}
              value={message}
              onChange={(e) => setMessage(e.target.value)}
              placeholder="Check the weather and my calendar, then send me a 3-line summary."
            />
          </Field>
          <Field label="When" hint={`Times are in ${localTimeZone()}.`}>
            <ScheduleInput preset={preset} onChange={setPreset} />
          </Field>
          <Field label="Where do results go?">
            <select className={inputClass} value={target} onChange={(e) => setTarget(e.target.value)}>
              <option value="">A new thread each run (listed under Recent runs)</option>
              {threads.map((t) => (
                <option key={t.key} value={t.key}>
                  Post into “{t.title}”
                </option>
              ))}
            </select>
          </Field>
          {startsInFolder && !target && folder && (
            <Field label="Works in" hint="Each run is a new Claude Code session in this folder.">
              <FolderPicker guy={guy} value={folder} onChange={setFolder} inline />
            </Field>
          )}
          {error && <p className="text-[12.5px] text-[var(--color-bad)]">{error}</p>}
        </div>
        <div className="mt-6 flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" onClick={save} disabled={saving}>
            {saving && <Spinner size={12} />} {isNew ? "Create routine" : "Save"}
          </Button>
        </div>
      </div>
    </Modal>
  );
}

function ScheduleInput({ preset, onChange }: { preset: Preset; onChange: (p: Preset) => void }) {
  const time = "time" in preset ? preset.time : "09:00";
  const kinds: Array<{ value: Preset["kind"]; label: string }> = [
    { value: "daily", label: "Every day" },
    { value: "weekdays", label: "Weekdays" },
    { value: "weekly", label: "Every week" },
    { value: "hourly", label: "Every few hours" },
    { value: "minutes", label: "Every few minutes" },
    { value: "once", label: "Just once" },
    { value: "custom", label: "Custom (cron)" },
  ];
  const switchKind = (kind: Preset["kind"]) => {
    switch (kind) {
      case "daily":
      case "weekdays":
        return onChange({ kind, time });
      case "weekly":
        return onChange({ kind, day: 1, time });
      case "hourly":
        return onChange({ kind, hours: 4 });
      case "minutes":
        return onChange({ kind, minutes: 30 });
      case "once": {
        const d = new Date(Date.now() + 3_600_000);
        const pad = (n: number) => String(n).padStart(2, "0");
        return onChange({ kind, at: `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:00` });
      }
      case "custom":
        return onChange({ kind, expr: "0 9 * * *" });
    }
  };
  return (
    <div className="flex flex-wrap items-center gap-2">
      <select className={`${inputBase} w-auto`} value={preset.kind} onChange={(e) => switchKind(e.target.value as Preset["kind"])}>
        {kinds.map((k) => (
          <option key={k.value} value={k.value}>
            {k.label}
          </option>
        ))}
      </select>
      {preset.kind === "weekly" && (
        <select className={`${inputBase} w-auto`} value={preset.day} onChange={(e) => onChange({ ...preset, day: Number(e.target.value) })}>
          {DAYS.map((d, i) => (
            <option key={d} value={i}>
              on {d}
            </option>
          ))}
        </select>
      )}
      {(preset.kind === "daily" || preset.kind === "weekdays" || preset.kind === "weekly") && (
        <>
          <span className="text-[var(--color-ink-3)]">at</span>
          <input type="time" className={`${inputBase} w-auto`} value={preset.time} onChange={(e) => onChange({ ...preset, time: e.target.value })} />
        </>
      )}
      {preset.kind === "hourly" && (
        <>
          <span className="text-[var(--color-ink-3)]">every</span>
          <input type="number" min={1} max={168} className={`${inputBase} w-20`} value={preset.hours} onChange={(e) => onChange({ ...preset, hours: Number(e.target.value) })} />
          <span className="text-[var(--color-ink-3)]">hours</span>
        </>
      )}
      {preset.kind === "minutes" && (
        <>
          <span className="text-[var(--color-ink-3)]">every</span>
          <input type="number" min={5} max={1440} className={`${inputBase} w-20`} value={preset.minutes} onChange={(e) => onChange({ ...preset, minutes: Number(e.target.value) })} />
          <span className="text-[var(--color-ink-3)]">minutes</span>
        </>
      )}
      {preset.kind === "once" && (
        <input type="datetime-local" className={`${inputBase} w-auto`} value={preset.at} onChange={(e) => onChange({ ...preset, at: e.target.value })} />
      )}
      {preset.kind === "custom" && (
        <input className={`${inputBase} w-44 font-mono`} value={preset.expr} onChange={(e) => onChange({ ...preset, expr: e.target.value })} placeholder="0 9 * * 1-5" />
      )}
    </div>
  );
}
