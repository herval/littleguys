import { Pencil, Plus, RotateCw, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import { needsAction, needsProxyFix, RUNTIME_LABEL, statusLabel } from "../lib/guys";
import { core, errorMessage, isTauri, listen } from "../lib/ipc";
import type { GatewayRoster, UpdateState } from "../lib/types";
import { useApp } from "../stores/app";
import { GuyMark } from "../ui/Avatar";
import { confirmAction } from "../ui/confirm";
import { Button, IconButton, inputClass, Modal } from "../ui/kit";
import { ClaudePluginFix, CredentialFix, needsClaudePlugin, PairingFix, ProxyFix } from "./GatewayFix";
import { GuyVisibility } from "./GuyVisibility";
import { Notifications } from "./Notifications";
import { Phones } from "./Phones";

export function Settings() {
  const close = () => useApp.getState().setSettingsOpen(false);
  const roster = useApp((s) => s.roster);
  const setAddGatewayOpen = useApp((s) => s.setAddGatewayOpen);
  const [autostart, setAutostart] = useState<boolean | null>(null);

  useEffect(() => {
    if (!isTauri) return;
    import("@tauri-apps/plugin-autostart").then((m) => m.isEnabled().then(setAutostart)).catch(() => {});
  }, []);

  const toggleAutostart = async (on: boolean) => {
    const m = await import("@tauri-apps/plugin-autostart");
    if (on) await m.enable();
    else await m.disable();
    setAutostart(await m.isEnabled());
  };

  return (
    <Modal onClose={close} width={580}>
      <div className="p-6">
        <div className="flex items-center gap-3">
          <GuyMark size={48} />
          <div>
            <h2 className="font-display text-[17px] font-semibold tracking-tight">Settings</h2>
            <p className="text-[12px] text-[var(--color-ink-3)]">littleguys · all your bots, in one place</p>
          </div>
        </div>

        <section className="mt-5">
          <div className="mb-1.5 flex items-center justify-between">
            <h3 className="text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">Bots & gateways</h3>
            <Button size="sm" onClick={() => setAddGatewayOpen(true)}>
              <Plus size={12} /> Add a bot
            </Button>
          </div>
          <div className="space-y-2">
            {roster.gateways.map((g) => (
              <GatewayRow key={g.id} gateway={g} />
            ))}
          </div>
        </section>

        <Phones />

        <GuyVisibility />

        <Notifications />

        <section className="mt-5">
          <h3 className="mb-1.5 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">General</h3>
          <div className="rounded-xl border border-[var(--color-line)] px-3.5 py-1.5">
            <Row label="Open at login">
              <input
                type="checkbox"
                className="accent-[var(--color-accent)]"
                disabled={autostart === null}
                checked={Boolean(autostart)}
                onChange={(e) => toggleAutostart(e.target.checked)}
              />
            </Row>
            <Row label="Open littleguys">
              <kbd className="rounded bg-[var(--color-chip)] px-1.5 py-0.5 text-[11px]">⇧⌘L</kbd>
            </Row>
            <UpdateRow />
          </div>
        </section>

        <div className="mt-6 flex justify-between">
          <Button variant="ghost" size="sm" onClick={() => core.quit()}>
            Quit littleguys
          </Button>
          <Button variant="primary" onClick={close}>
            Done
          </Button>
        </div>
      </div>
    </Modal>
  );
}

function GatewayRow({ gateway: g }: { gateway: GatewayRoster }) {
  const [renaming, setRenaming] = useState(false);
  const [name, setName] = useState(g.name);
  const [error, setError] = useState<string | null>(null);
  const isConfig = g.id === "openclaw";
  const phase = g.status.phase;
  const dot = !g.enabled
    ? "bg-[var(--color-ink-3)]/40"
    : phase === "connected"
      ? "bg-[var(--color-ok)]"
      : needsAction(g.status)
        ? "bg-[var(--color-warn)]"
        : phase === "connecting" || phase === "starting"
          ? "bg-[var(--color-warn)] animate-pulse"
          : "bg-[var(--color-bad)]";

  const run = (fn: () => Promise<unknown>) => fn().catch((e) => setError(errorMessage(e)));
  // Scopes are OpenClaw's pairing grants; other runtimes report none.
  const missingAdmin = g.status.kind === "openclaw" && phase === "connected" && !g.status.scopes.includes("operator.admin");

  return (
    <div className={`rounded-xl border border-[var(--color-line)] p-3 ${g.enabled ? "" : "opacity-60"}`}>
      <div className="flex items-center gap-2.5">
        <span className={`h-2 w-2 shrink-0 rounded-full ${dot}`} />
        <div className="min-w-0 flex-1">
          {renaming ? (
            <input
              autoFocus
              className={`${inputClass} h-7 py-0`}
              value={name}
              onChange={(e) => setName(e.target.value)}
              onBlur={() => {
                setRenaming(false);
                if (name.trim() !== g.name) run(() => core.gateways.update(g.id, { name: name.trim() }));
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                if (e.key === "Escape") (setName(g.name), setRenaming(false));
              }}
            />
          ) : (
            <p className="flex items-center gap-1.5 font-medium">
              {g.name}
              {isConfig ? (
                <span className="rounded bg-[var(--color-chip)] px-1.5 py-0.5 text-[10px] font-medium text-[var(--color-ink-3)]">
                  from ~/.openclaw
                </span>
              ) : (
                <span className="rounded bg-[var(--color-chip)] px-1.5 py-0.5 text-[10px] font-medium text-[var(--color-ink-3)]">
                  {RUNTIME_LABEL[g.status.kind] ?? "OpenClaw"}
                </span>
              )}
            </p>
          )}
          <p className="truncate font-mono text-[11px] text-[var(--color-ink-3)]">
            {g.status.url ?? "—"}
            {g.status.serverVersion ? ` · v${g.status.serverVersion}` : ""}
          </p>
        </div>
        <span className="shrink-0 text-[11.5px] text-[var(--color-ink-3)]">
          {g.enabled
            ? phase === "connected" && g.loaded
              ? g.status.kind === "acp"
                ? "Ready"
                : g.status.kind === "claudecode"
                  ? `${g.sessions.length} ${g.sessions.length === 1 ? "session" : "sessions"}`
                  : `${g.agents.length} ${g.agents.length === 1 ? "guy" : "guys"}`
              : statusLabel(g.status)
            : "Off"}
        </span>
        <input
          type="checkbox"
          title={g.enabled ? "Turn off" : "Turn on"}
          className="accent-[var(--color-accent)]"
          checked={g.enabled}
          onChange={(e) => run(() => core.gateways.update(g.id, { enabled: e.target.checked }))}
        />
        <IconButton label="Rename" onClick={() => setRenaming(true)}>
          <Pencil size={12} />
        </IconButton>
        <IconButton label="Reconnect" onClick={() => core.reconnect(g.id)} disabled={!g.enabled}>
          <RotateCw size={12} />
        </IconButton>
        {!isConfig && (
          <IconButton
            label="Remove"
            onClick={async () => {
              const what =
                g.status.kind === "acp"
                  ? `Remove ${g.name}? Its chat history in littleguys goes too.`
                  : g.status.kind === "claudecode"
                    ? `Remove ${g.name}? Your sessions keep running; they just won't show up here.`
                    : `Remove ${g.name}? Its little guys stay where they are; they just won't show up here.`;
              if (await confirmAction(what, "littleguys", "Remove"))
                run(() => core.gateways.remove(g.id));
            }}
          >
            <Trash2 size={12} />
          </IconButton>
        )}
      </div>
      {g.enabled && phase === "pairing" && (
        <div className="mt-3 border-t border-[var(--color-line)] pt-3">
          <PairingFix status={g.status} compact />
        </div>
      )}
      {g.enabled && phase === "auth-failed" && (
        <div className="mt-3 border-t border-[var(--color-line)] pt-3">
          <CredentialFix status={g.status} />
        </div>
      )}
      {g.enabled && needsProxyFix(g.status) && (
        <div className="mt-3 border-t border-[var(--color-line)] pt-3">
          <ProxyFix status={g.status} compact />
        </div>
      )}
      {g.enabled && needsClaudePlugin(g.status) && (
        <div className="mt-3 border-t border-[var(--color-line)] pt-3">
          <ClaudePluginFix status={g.status} />
        </div>
      )}
      {g.status.kind === "claudecode" && <ClaudeApprovals gateway={g} onError={setError} />}
      {g.enabled &&
        (phase === "offline" || phase === "error" || phase === "no-config") &&
        g.status.message &&
        !needsProxyFix(g.status) &&
        !needsClaudePlugin(g.status) && (
        <p className="mt-2 text-[11.5px] text-[var(--color-ink-3)]">{g.status.message}</p>
      )}
      {missingAdmin && (
        <p className="mt-2 rounded-lg bg-[var(--color-warn)]/12 px-3 py-2 text-[11.5px] text-[var(--color-ink-2)]">
          Approved without admin access: chatting works, but making guys and routines here won't.
        </p>
      )}
      {error && <p className="mt-2 text-[11.5px] text-[var(--color-bad)]">{error}</p>}
    </div>
  );
}

/** Claude Code's one setting: whether littleguys answers background sessions' permission prompts. */
function ClaudeApprovals({ gateway: g, onError }: { gateway: GatewayRoster; onError: (e: string) => void }) {
  const on = g.status.capabilities.approvals;
  return (
    <label className="mt-3 flex items-start gap-2.5 border-t border-[var(--color-line)] pt-3 text-[12.5px]">
      <input
        type="checkbox"
        className="mt-[3px] accent-[var(--color-accent)]"
        checked={on}
        onChange={(e) => core.bots.claudeConfigure(g.id, { approvals: e.target.checked }).catch((err) => onError(errorMessage(err)))}
      />
      <span>
        <span className="font-medium">Answer permission prompts here</span>
        <span className="block text-[11.5px] text-[var(--color-ink-3)]">
          Background sessions wait for your Allow or Deny in littleguys. Terminal sessions keep asking in their terminal.
        </span>
      </span>
    </label>
  );
}

function UpdateRow() {
  const [update, setUpdate] = useState<UpdateState | null>(null);

  useEffect(() => {
    core.updates.state().then(setUpdate).catch(() => {});
    const off = listen<UpdateState>("lg:update", setUpdate);
    return () => void off.then((f) => f());
  }, []);

  if (!update) return null;
  const check = () => core.updates.check().then(setUpdate).catch(() => {});
  return (
    <Row label={`Version ${update.current}`}>
      {update.phase === "ready" ? (
        <Button size="sm" variant="primary" onClick={() => core.updates.restart()}>
          Restart to update to {update.version}
        </Button>
      ) : update.phase === "downloading" ? (
        <span className="text-[var(--color-ink-3)]">Downloading {update.version}…</span>
      ) : update.phase === "checking" ? (
        <span className="text-[var(--color-ink-3)]">Checking for updates…</span>
      ) : update.phase === "disabled" ? (
        <span className="text-[var(--color-ink-3)]">Dev build: no updates</span>
      ) : (
        <span className="flex items-center gap-2">
          {update.phase === "error" ? (
            <span className="truncate text-[var(--color-bad)]" title={update.error ?? undefined}>
              Couldn't check
            </span>
          ) : (
            update.checkedAt && <span className="text-[var(--color-ink-3)]">Up to date</span>
          )}
          <Button size="sm" variant="ghost" onClick={check}>
            Check for updates
          </Button>
        </span>
      )}
    </Row>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 border-b border-[var(--color-line)] py-2 text-[12.5px] last:border-b-0">
      <span className="text-[var(--color-ink-2)]">{label}</span>
      <span className="min-w-0 truncate text-right">{children}</span>
    </div>
  );
}
