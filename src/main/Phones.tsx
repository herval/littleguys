import { Bell, Pencil, QrCode, Smartphone, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import { bridge, type BridgeState, type PhoneState } from "../lib/bridge";
import { relativeTime } from "../lib/guys";
import { errorMessage } from "../lib/ipc";
import { confirmAction } from "../ui/confirm";
import { Button, IconButton, inputClass } from "../ui/kit";
import { PairPhone } from "./PairPhone";

/** Settings section: the iPhones paired with this Mac, and pairing another. */
export function Phones() {
  const [state, setState] = useState<BridgeState | null>(null);
  const [pairing, setPairing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    bridge.state().then(setState).catch(() => {});
    const off = bridge.onState(setState);
    return () => void off.then((f) => f());
  }, []);

  if (!state) return null;
  const run = (fn: () => Promise<unknown>) => {
    setError(null);
    fn().catch((e) => setError(errorMessage(e)));
  };

  return (
    <section className="mt-5">
      <div className="mb-1.5 flex items-center justify-between">
        <h3 className="text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">Phones</h3>
        <Button size="sm" onClick={() => setPairing(true)}>
          <QrCode size={12} /> Pair mobile app
        </Button>
      </div>
      {state.phones.length === 0 ? (
        <p className="rounded-xl border border-dashed border-[var(--color-line)] px-3.5 py-3 text-[12px] text-[var(--color-ink-3)]">
          Use your guys from your iPhone. Your phone connects to this Mac over Wi-Fi, or from anywhere with Tailscale.
        </p>
      ) : (
        <div className="rounded-xl border border-[var(--color-line)]">
          {state.phones.map((p, i) => (
            <PhoneRow key={p.key} phone={p} first={i === 0} enabled={state.enabled} run={run} />
          ))}
          <label className="flex items-center gap-2.5 border-t border-[var(--color-line)] px-3 py-2 text-[12.5px]">
            <input
              type="checkbox"
              className="accent-[var(--color-accent)]"
              checked={state.enabled}
              onChange={(e) => run(() => bridge.setEnabled(e.target.checked))}
            />
            Let paired phones connect
          </label>
        </div>
      )}
      {error && <p className="mt-2 text-[11.5px] text-[var(--color-bad)]">{error}</p>}
      {pairing && <PairPhone state={state} onClose={() => setPairing(false)} />}
    </section>
  );
}

function PhoneRow({
  phone: p,
  first,
  enabled,
  run,
}: {
  phone: PhoneState;
  first: boolean;
  enabled: boolean;
  run: (fn: () => Promise<unknown>) => void;
}) {
  const [renaming, setRenaming] = useState(false);
  const [name, setName] = useState(p.name);
  const status = p.connected
    ? p.via === "tailscale"
      ? "Connected over Tailscale"
      : p.via === "local"
        ? "Connected on this Mac"
        : "Connected over Wi-Fi"
    : !enabled
      ? "Can't connect while phones are turned off"
      : p.lastSeenAtMs
        ? `Last seen ${relativeTime(p.lastSeenAtMs)}`
        : "Not connected yet";

  return (
    <div className={`flex items-center gap-2.5 px-3 py-2 ${first ? "" : "border-t border-[var(--color-line)]"}`}>
      <Smartphone size={16} className="shrink-0 text-[var(--color-ink-3)]" />
      <div className="min-w-0 flex-1">
        {renaming ? (
          <input
            autoFocus
            className={`${inputClass} h-7 py-0`}
            value={name}
            onChange={(e) => setName(e.target.value)}
            onBlur={() => {
              setRenaming(false);
              if (name.trim() && name.trim() !== p.name) run(() => bridge.renamePhone(p.key, name.trim()));
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") (e.target as HTMLInputElement).blur();
              if (e.key === "Escape") (setName(p.name), setRenaming(false));
            }}
          />
        ) : (
          <p className="flex items-center gap-1.5 truncate text-[12.5px] font-medium">
            {p.name}
            {p.push && <Bell size={11} className="shrink-0 text-[var(--color-ink-3)]" aria-label="Gets notifications" />}
          </p>
        )}
        <p className="truncate text-[11px] text-[var(--color-ink-3)]">
          <span className={`mr-1.5 inline-block h-1.5 w-1.5 rounded-full align-middle ${p.connected ? "bg-[var(--color-ok)]" : "bg-[var(--color-ink-3)]/40"}`} />
          {status}
        </p>
      </div>
      <IconButton label="Rename" onClick={() => setRenaming(true)}>
        <Pencil size={12} />
      </IconButton>
      <IconButton
        label="Remove"
        onClick={async () => {
          if (await confirmAction(`Remove ${p.name}? It won't reach your guys through this Mac until you pair it again.`, "littleguys", "Remove"))
            run(() => bridge.removePhone(p.key));
        }}
      >
        <Trash2 size={12} />
      </IconButton>
    </div>
  );
}
