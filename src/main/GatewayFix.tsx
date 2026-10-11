// What a gateway can need from you: approve this Mac, a token, or a setting
// changed on the gateway host.
import { Check, Copy } from "lucide-react";
import { useState } from "react";
import { core, errorMessage } from "../lib/ipc";
import type { ConnStatus } from "../lib/types";
import { Button, Field, inputBase, inputClass, Spinner } from "../ui/kit";

export function hostOf(url: string | null | undefined): string {
  if (!url) return "your gateway";
  try {
    return new URL(url.replace(/^ws/, "http")).hostname;
  } catch {
    return url;
  }
}

export function CopyLine({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="mt-2 flex items-center gap-2 rounded-lg bg-[var(--color-chip)] py-1.5 pl-3 pr-1.5 font-mono text-[12px]">
      <span className="selectable min-w-0 flex-1 truncate">{text}</span>
      <button
        onClick={() => {
          navigator.clipboard.writeText(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        }}
        className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md hover:bg-black/5 dark:hover:bg-white/10"
        aria-label="Copy"
      >
        {copied ? <Check size={13} className="text-[var(--color-ok)]" /> : <Copy size={13} />}
      </button>
    </div>
  );
}

export function PairingFix({ status, compact = false }: { status: ConnStatus; compact?: boolean }) {
  const command = status.requestId ? `openclaw devices approve ${status.requestId}` : "openclaw devices list";
  return (
    <div>
      <p className={compact ? "text-[12px]" : "font-medium"}>
        On <span className="font-medium">{status.gatewayName || hostOf(status.url)}</span>, run:
      </p>
      <CopyLine text={command} />
      <p className="mt-2 flex items-center gap-1.5 text-[11.5px] text-[var(--color-ink-3)]">
        <Spinner size={11} /> Waiting for approval — this updates by itself.
      </p>
      {!compact && (
        <p className="mt-1 text-[11.5px] text-[var(--color-ink-3)]">
          This Mac shows up as “littleguys” (device {status.deviceId.slice(0, 8)}…). You can also approve it from OpenClaw's Control UI.
        </p>
      )}
    </div>
  );
}

/**
 * The gateway sits behind a Tailscale Serve route it doesn't run itself, so it
 * won't take connections through it until it trusts the route's (loopback)
 * proxy. Letting OpenClaw run the route instead also works, but only on a
 * loopback-only gateway.bind, so it isn't offered here.
 */
export function ProxyFix({ status, compact = false }: { status: ConnStatus; compact?: boolean }) {
  const host = status.gatewayName || hostOf(status.url);
  return (
    <div>
      <p className={compact ? "text-[12px]" : "font-medium"}>
        On <span className="font-medium">{host}</span>, run:
      </p>
      <CopyLine text={`openclaw config set gateway.trustedProxies '["127.0.0.1"]' --strict-json`} />
      <CopyLine text="openclaw gateway restart" />
      <p className="mt-2 text-[11.5px] text-[var(--color-ink-3)]">
        {host} is reachable through a Tailscale Serve route that OpenClaw doesn't run, so it turns away everything that comes through it,
        before a token or setup code is even sent. This tells it to trust that route; its token and device approval still apply.
      </p>
      <p className="mt-2 flex items-center gap-1.5 text-[11.5px] text-[var(--color-ink-3)]">
        <Spinner size={11} /> littleguys tries again every 30 seconds; you'll still approve this Mac once.
      </p>
    </div>
  );
}

/** Claude Code without the littleguys plugin (never installed, turned off, or installed by another littleguys). */
export function needsClaudePlugin(status: ConnStatus): boolean {
  return status.kind === "claudecode" && status.detailCode === "claude-plugin";
}

export function ClaudePluginFix({ status }: { status: ConnStatus }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const install = async () => {
    setBusy(true);
    setError(null);
    try {
      await core.bots.claudeInstallPlugin();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div>
      <p className="text-[12.5px]">{status.message}</p>
      <Button size="sm" variant="primary" className="mt-2" disabled={busy} onClick={install}>
        {busy && <Spinner size={11} />} Install the plugin
      </Button>
      {error && <p className="mt-2 text-[11.5px] text-[var(--color-bad)]">{error}</p>}
    </div>
  );
}

export function CredentialFix({ status }: { status: ConnStatus }) {
  const [value, setValue] = useState("");
  const [kind, setKind] = useState<"token" | "password">("token");
  // A logged-out local agent (e.g. Claude Code) needs its own login, not a token.
  if (status.kind === "acp" && !(status.url ?? "").startsWith("ws")) {
    return <p className="text-[12.5px]">{status.message ?? "This agent needs you to log in to it first."}</p>;
  }
  if (status.kind === "hermes") {
    return (
      <Field
        label={`${status.gatewayName || "Hermes"} API key`}
        hint={<>The API_SERVER_KEY from that machine's ~/.hermes/.env. Kept on this Mac only.</>}
      >
        <div className="flex gap-2">
          <input type="password" className={inputClass} value={value} onChange={(e) => setValue(e.target.value)} placeholder="API key" />
          <Button variant="primary" disabled={!value.trim()} onClick={() => core.setCredential(status.gatewayId, "token", value.trim())}>
            Connect
          </Button>
        </div>
      </Field>
    );
  }
  return (
    <div>
      <Field
        label={`${status.gatewayName || "Gateway"} ${kind}`}
        hint={
          <>
            On that machine: <span className="font-mono">openclaw config get gateway.auth.token</span>. Kept on this Mac only, and only
            needed once — after approval littleguys uses its own device key.
          </>
        }
      >
        <div className="flex gap-2">
          <input
            type="password"
            className={inputClass}
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder={kind === "token" ? "Paste the gateway token" : "Gateway password"}
          />
          <select className={`${inputBase} w-auto`} value={kind} onChange={(e) => setKind(e.target.value as "token" | "password")}>
            <option value="token">Token</option>
            <option value="password">Password</option>
          </select>
          <Button variant="primary" disabled={!value.trim()} onClick={() => core.setCredential(status.gatewayId, kind, value.trim())}>
            Connect
          </Button>
        </div>
      </Field>
      {status.message && <p className="mt-2 text-[11.5px] text-[var(--color-ink-3)]">Gateway said: {status.message}</p>}
      <SetupCodeFix />
    </div>
  );
}

/** Paste a setup code from OpenClaw (Control UI pairing dialog or `openclaw qr --setup-code-only`). */
export function SetupCodeFix({ onConnected }: { onConnected?: (gatewayId: string) => void }) {
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const id = await core.gateways.useSetupCode(code.trim());
      setDone(true);
      setCode("");
      onConnected?.(id);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="mt-3 border-t border-[var(--color-line)] pt-3">
      <Field
        label="Or use a setup code"
        hint={
          <>
            From OpenClaw's Control UI (Devices → pair) or <span className="font-mono">openclaw qr --setup-code-only</span> on the gateway. Codes
            expire after ~10 minutes; you'll still approve this Mac once.
          </>
        }
      >
        <div className="flex gap-2">
          <input
            className={`${inputClass} font-mono`}
            value={code}
            onChange={(e) => setCode(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && code.trim()) submit();
            }}
            placeholder="Paste the setup code"
          />
          <Button variant="primary" disabled={busy || !code.trim()} onClick={submit}>
            {busy ? <Spinner size={11} /> : null} Use code
          </Button>
        </div>
      </Field>
      {done && <p className="mt-1.5 text-[11.5px] text-[var(--color-ok)]">Connecting with the setup code…</p>}
      {error && <p className="mt-1.5 text-[11.5px] text-[var(--color-bad)]">{error}</p>}
    </div>
  );
}
