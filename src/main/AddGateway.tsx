import { ChevronRight, Globe, KeyRound, Network, RefreshCw, RotateCw, Trash2, Wifi } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { gatewayOf, needsProxyFix, phaseLabel } from "../lib/guys";
import { core, errorMessage } from "../lib/ipc";
import type { DiscoveredGateway, GatewayRoster } from "../lib/types";
import { useApp } from "../stores/app";
import { confirmAction } from "../ui/confirm";
import { Button, IconButton, inputBase, inputClass, Modal, Segmented, Spinner } from "../ui/kit";
import { AcpTab, ClaudeCodeTab, HermesTab } from "./AddBotTabs";
import { ClaudePluginFix, CredentialFix, needsClaudePlugin, PairingFix, ProxyFix } from "./GatewayFix";

type Tab = "openclaw" | "hermes" | "claude" | "acp";

const sectionLabel = "text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]";

/**
 * Adding a bot: an OpenClaw gateway, a Hermes install, Claude Code (every
 * session on this Mac), or an ACP agent (Codex, Gemini CLI…). Then it walks you through whatever the
 * new bot needs (approval, token, login).
 */
export function AddGateway() {
  const close = () => useApp.getState().setAddGatewayOpen(false);
  const [tab, setTab] = useState<Tab>("openclaw");
  const [openId, setOpenId] = useState<string | null>(null);
  return (
    <Modal onClose={close} width={580}>
      <div className="p-6">
        {openId ? (
          <GatewayStatus id={openId} onDone={close} onBack={() => setOpenId(null)} />
        ) : (
          <>
            <h2 className="font-display text-[17px] font-semibold tracking-tight">Add a bot</h2>
            <p className="mt-0.5 mb-4 text-[12.5px] text-[var(--color-ink-3)]">
              Every bot you add brings its little guys into this app.
            </p>
            <Segmented
              value={tab}
              onChange={setTab}
              options={[
                { value: "openclaw", label: "OpenClaw" },
                { value: "hermes", label: "Hermes" },
                { value: "claude", label: "Claude Code" },
                { value: "acp", label: "Agents (ACP)" },
              ]}
            />
            <div className="mt-5">
              {tab === "openclaw" && <OpenClawTab onOpen={setOpenId} />}
              {tab === "hermes" && <HermesTab onAdded={setOpenId} />}
              {tab === "claude" && <ClaudeCodeTab onAdded={setOpenId} />}
              {tab === "acp" && <AcpTab onAdded={setOpenId} />}
            </div>
            <div className="mt-6 flex justify-end">
              <Button variant="ghost" onClick={close}>
                Close
              </Button>
            </div>
          </>
        )}
      </div>
    </Modal>
  );
}

type Tone = "ok" | "warn" | "bad" | "off";

const TONE_TEXT: Record<Tone, string> = {
  ok: "text-[var(--color-ok)]",
  warn: "text-[var(--color-warn)]",
  bad: "text-[var(--color-bad)]",
  off: "text-[var(--color-ink-3)]",
};
const TONE_DOT: Record<Tone, string> = {
  ok: "bg-[var(--color-ok)]",
  warn: "bg-[var(--color-warn)]",
  bad: "bg-[var(--color-bad)]",
  off: "bg-[var(--color-ink-3)]/50",
};

/** What a saved gateway is actually doing — not just "it's in the list". */
function statusOf(g: GatewayRoster | undefined): { tone: Tone; label: string } {
  if (!g) return { tone: "warn", label: "Connecting…" };
  if (!g.enabled) return { tone: "off", label: "Turned off" };
  switch (g.status.phase) {
    case "connected":
      return {
        tone: "ok",
        label: g.loaded ? `Connected · ${g.agents.length} ${g.agents.length === 1 ? "guy" : "guys"}` : "Connected",
      };
    case "pairing":
    case "auth-failed":
      return { tone: "warn", label: phaseLabel(g.status.phase) };
    case "error":
      if (needsProxyFix(g.status)) return { tone: "warn", label: "Needs a setting" };
      return { tone: "bad", label: "Can't connect" };
    case "offline":
    case "no-config":
      return { tone: "bad", label: "Can't connect" };
    default:
      return { tone: "warn", label: "Connecting…" };
  }
}

/** Removes a saved gateway. Only asks first when it's actually working. */
async function removeGateway(id: string): Promise<boolean> {
  const g = gatewayOf(useApp.getState().roster, id);
  if (
    g?.status.phase === "connected" &&
    !(await confirmAction(`Remove ${g.name}? Its little guys stay where they are; they just won't show up here.`, "littleguys", "Remove"))
  )
    return false;
  await core.gateways.remove(id);
  return true;
}

function OpenClawTab({ onOpen }: { onOpen: (id: string) => void }) {
  const [found, setFound] = useState<DiscoveredGateway[] | null>(null);
  const [searching, setSearching] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busyUrl, setBusyUrl] = useState<string | null>(null);

  const search = useCallback(async () => {
    setSearching(true);
    try {
      setFound(await core.gateways.discover());
    } catch {
      setFound([]);
    } finally {
      setSearching(false);
    }
  }, []);

  useEffect(() => {
    search();
  }, [search]);

  const add = async (d: DiscoveredGateway) => {
    setError(null);
    setBusyUrl(d.url);
    try {
      onOpen((await core.gateways.add(d.url, d.name)).id);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusyUrl(null);
    }
  };

  const remove = async (d: DiscoveredGateway) => {
    setError(null);
    try {
      if (d.addedAs && (await removeGateway(d.addedAs)))
        setFound((f) => f?.map((x) => (x.url === d.url ? { ...x, addedAs: null } : x)) ?? null);
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  return (
    <>
      <div className="mb-2 flex items-center justify-between">
        <span className={sectionLabel}>Found on your network</span>
        <button
          onClick={search}
          disabled={searching}
          className="flex items-center gap-1 text-[11.5px] text-[var(--color-ink-3)] hover:text-[var(--color-ink)] disabled:opacity-50"
        >
          <RefreshCw size={11} className={searching ? "animate-spin" : ""} /> Search again
        </button>
      </div>
      <div className="rounded-xl border border-[var(--color-line)]">
        {found === null || (searching && found.length === 0) ? (
          <div className="flex items-center gap-2 px-3.5 py-4 text-[12.5px] text-[var(--color-ink-3)]">
            <Spinner /> Looking on your tailnet and local network…
          </div>
        ) : found.length === 0 ? (
          <p className="px-3.5 py-4 text-[12.5px] text-[var(--color-ink-3)]">
            Didn't find any. Gateways show up here when they're on your Tailscale network (via Tailscale Serve) or advertise on your LAN
            (Bonjour). You can always connect one by address below.
          </p>
        ) : (
          found.map((d, i) => (
            <FoundRow
              key={d.url}
              found={d}
              first={i === 0}
              busy={busyUrl === d.url}
              disabled={busyUrl !== null}
              onAdd={() => add(d)}
              onOpen={() => d.addedAs && onOpen(d.addedAs)}
              onRemove={() => remove(d)}
            />
          ))
        )}
      </div>
      {error && <p className="mt-2 text-[12px] text-[var(--color-bad)]">{error}</p>}

      <ConnectOther onOpen={onOpen} />
    </>
  );
}

function FoundRow({
  found: d,
  first,
  busy,
  disabled,
  onAdd,
  onOpen,
  onRemove,
}: {
  found: DiscoveredGateway;
  first: boolean;
  busy: boolean;
  disabled: boolean;
  onAdd: () => void;
  onOpen: () => void;
  onRemove: () => void;
}) {
  const gateway = useApp((s) => (d.addedAs ? gatewayOf(s.roster, d.addedAs) : undefined));
  const status = d.addedAs ? statusOf(gateway) : null;
  return (
    <div className={`flex items-center gap-3 px-3.5 py-2.5 ${first ? "" : "border-t border-[var(--color-line)]"}`}>
      <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-[var(--color-chip)] text-[var(--color-ink-2)]">
        {d.via === "tailscale" ? <Network size={15} /> : <Wifi size={15} />}
      </span>
      <div className="min-w-0 flex-1">
        <p className="font-medium">{d.name}</p>
        <p className="truncate font-mono text-[11px] text-[var(--color-ink-3)]">
          {d.url} · {d.via === "tailscale" ? "Tailscale" : "local network"}
        </p>
      </div>
      {status ? (
        <>
          {status.tone === "ok" || status.tone === "off" ? (
            <span className={`flex shrink-0 items-center gap-1.5 text-[12px] ${TONE_TEXT[status.tone]}`}>
              <span className={`h-1.5 w-1.5 rounded-full ${TONE_DOT[status.tone]}`} /> {status.label}
            </span>
          ) : (
            <button
              onClick={onOpen}
              title="See what's wrong"
              className={`flex shrink-0 items-center gap-1.5 rounded-md px-1.5 py-1 text-[12px] font-medium transition hover:bg-[var(--color-chip)] ${TONE_TEXT[status.tone]}`}
            >
              <span className={`h-1.5 w-1.5 rounded-full ${TONE_DOT[status.tone]}`} /> {status.label}
              <ChevronRight size={12} />
            </button>
          )}
          {d.addedAs === "openclaw" ? (
            // Comes from ~/.openclaw: it can be turned off in Settings, not removed.
            <span className="w-7 shrink-0" />
          ) : (
            <IconButton label={`Remove ${d.name}`} onClick={onRemove}>
              <Trash2 size={12} />
            </IconButton>
          )}
        </>
      ) : (
        <Button size="sm" variant="primary" disabled={disabled} onClick={onAdd}>
          {busy && <Spinner size={11} />} Add
        </Button>
      )}
    </div>
  );
}

/** Connect a gateway that wasn't discovered: by address, or with a setup code. */
function ConnectOther({ onOpen }: { onOpen: (id: string) => void }) {
  const [how, setHow] = useState<"address" | "code">("address");
  const [url, setUrl] = useState("");
  const [code, setCode] = useState("");
  const [more, setMore] = useState(false);
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"token" | "password">("token");
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const value = how === "address" ? url.trim() : code.trim();
  const submit = async () => {
    if (!value || busy) return;
    setBusy(true);
    setError(null);
    try {
      if (how === "address") {
        const entry = await core.gateways.add(value, name.trim() || undefined, secret ? kind : undefined, secret || undefined);
        onOpen(entry.id);
      } else {
        onOpen(await core.gateways.useSetupCode(value));
      }
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="mt-6">
      <div className="mb-2 flex items-center justify-between">
        <span className={sectionLabel}>Connect another gateway</span>
        <div className="flex rounded-md bg-[var(--color-chip)] p-0.5">
          {(["address", "code"] as const).map((h) => (
            <button
              key={h}
              onClick={() => (setHow(h), setError(null))}
              className={`rounded px-2 py-0.5 text-[11.5px] font-medium transition ${
                how === h
                  ? "bg-[var(--color-pane-solid)] text-[var(--color-ink)] shadow-sm"
                  : "text-[var(--color-ink-3)] hover:text-[var(--color-ink)]"
              }`}
            >
              {h === "address" ? "By address" : "With a setup code"}
            </button>
          ))}
        </div>
      </div>

      <div className="flex gap-2">
        <div className="relative flex-1">
          {how === "address" ? (
            <Globe size={14} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-[var(--color-ink-3)]" />
          ) : (
            <KeyRound size={14} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-[var(--color-ink-3)]" />
          )}
          <input
            key={how}
            className={`${inputClass} pl-8 ${how === "code" ? "font-mono" : ""}`}
            value={how === "address" ? url : code}
            onChange={(e) => (how === "address" ? setUrl : setCode)(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()}
            placeholder={how === "address" ? "studio.your-tailnet.ts.net or 192.168.1.20:18789" : "Paste the setup code"}
          />
        </div>
        <Button variant="primary" disabled={busy || !value} onClick={submit}>
          {busy && <Spinner size={11} />} Connect
        </Button>
      </div>

      {how === "address" ? (
        <>
          <button
            onClick={() => setMore(!more)}
            className="mt-2 flex items-center gap-1 text-[11.5px] text-[var(--color-ink-3)] hover:text-[var(--color-ink-2)]"
          >
            <ChevronRight size={12} className={`transition ${more ? "rotate-90" : ""}`} /> Name and token (optional)
          </button>
          {more && (
            <div className="mt-2 flex gap-2">
              <input className={`${inputBase} w-36`} value={name} onChange={(e) => setName(e.target.value)} placeholder="Name" />
              <input
                type="password"
                className={inputClass}
                value={secret}
                onChange={(e) => setSecret(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && submit()}
                placeholder={kind === "token" ? "Gateway token" : "Gateway password"}
              />
              <select className={`${inputBase} w-auto`} value={kind} onChange={(e) => setKind(e.target.value as "token" | "password")}>
                <option value="token">Token</option>
                <option value="password">Password</option>
              </select>
            </div>
          )}
        </>
      ) : (
        <p className="mt-2 text-[11.5px] text-[var(--color-ink-3)]">
          From OpenClaw's Control UI (Devices → Pair), or run <span className="font-mono">openclaw qr --setup-code-only</span> on the gateway.
          Codes expire after about 10 minutes.
        </p>
      )}
      {error && <p className="mt-2 text-[12px] text-[var(--color-bad)]">{error}</p>}
    </section>
  );
}

/** Live status of one saved gateway: approval, token, why it can't connect — or done. */
function GatewayStatus({ id, onDone, onBack }: { id: string; onDone: () => void; onBack: () => void }) {
  const gateway = useApp((s) => gatewayOf(s.roster, id));
  const [error, setError] = useState<string | null>(null);
  if (!gateway) return <Spinner />;
  const { status } = gateway;
  const connected = status.phase === "connected";
  const failed = status.phase === "offline" || status.phase === "error" || status.phase === "no-config";
  const remove = async () => {
    setError(null);
    try {
      if (await removeGateway(id)) onBack();
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  return (
    <div>
      <h2 className="font-display text-[17px] font-semibold tracking-tight">{gateway.name}</h2>
      <p className="font-mono text-[11.5px] text-[var(--color-ink-3)]">{status.url}</p>
      <div className="mt-5 rounded-xl border border-[var(--color-line)] p-4">
        {connected ? (
          <p className="flex items-center gap-2 font-medium text-[var(--color-ok)]">
            <span className="h-2 w-2 rounded-full bg-[var(--color-ok)]" />
            Connected
            {gateway.loaded && (
              <span className="font-normal text-[var(--color-ink-2)]">
                {status.kind === "claudecode"
                  ? ` — following ${gateway.sessions.length} ${gateway.sessions.length === 1 ? "session" : "sessions"}`
                  : ` — ${gateway.agents.length} little ${gateway.agents.length === 1 ? "guy" : "guys"} joined`}
              </span>
            )}
          </p>
        ) : needsClaudePlugin(status) ? (
          <ClaudePluginFix status={status} />
        ) : status.phase === "pairing" ? (
          <PairingFix status={status} />
        ) : needsProxyFix(status) ? (
          <ProxyFix status={status} />
        ) : status.phase === "auth-failed" ? (
          <>
            <p className="mb-3 text-[12.5px]">This gateway needs its token (or password) the first time.</p>
            <CredentialFix status={status} />
          </>
        ) : failed ? (
          <div>
            <p className="flex items-center gap-2 font-medium text-[var(--color-bad)]">
              <span className="h-2 w-2 rounded-full bg-[var(--color-bad)]" />
              Can't connect
            </p>
            {status.message && <p className="selectable mt-1.5 break-words text-[12px] text-[var(--color-ink-2)]">{status.message}</p>}
            <p className="mt-1.5 text-[11.5px] text-[var(--color-ink-3)]">littleguys keeps retrying in the background until you remove it.</p>
            <Button size="sm" className="mt-3" onClick={() => core.reconnect(id)}>
              <RotateCw size={12} /> Try again now
            </Button>
          </div>
        ) : (
          <p className="flex items-center gap-2 text-[12.5px] text-[var(--color-ink-3)]">
            <Spinner /> Connecting…
          </p>
        )}
      </div>
      {error && <p className="mt-2 text-[12px] text-[var(--color-bad)]">{error}</p>}
      <div className="mt-6 flex items-center gap-2">
        <Button variant="ghost" onClick={onBack}>
          ← Back
        </Button>
        <span className="flex-1" />
        {!connected && id !== "openclaw" && (
          <Button variant="danger" onClick={remove}>
            <Trash2 size={12} /> Remove
          </Button>
        )}
        <Button variant="primary" onClick={onDone}>
          {connected ? "Done" : "Finish later"}
        </Button>
      </div>
    </div>
  );
}
