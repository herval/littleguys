import { ChevronRight, Plus, RotateCw, Stethoscope } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { core, errorMessage, listen } from "../lib/ipc";
import type { ConnStatus, DoctorIssue, DoctorReport } from "../lib/types";
import { needsProxyFix, statusLabel } from "../lib/guys";
import { useApp } from "../stores/app";
import { GuyMark } from "../ui/Avatar";
import { Button, Field, inputClass, Spinner } from "../ui/kit";
import { CredentialFix, hostOf, PairingFix, ProxyFix } from "./GatewayFix";

/**
 * Shown whenever there's no live gateway connection: explains what's wrong in
 * plain words and offers the one-click fix (install, update, start, set up,
 * approve, paste token).
 */
export function Doctor() {
  const roster = useApp((s) => s.roster);
  const setAddGatewayOpen = useApp((s) => s.setAddGatewayOpen);
  // The gateway from ~/.openclaw is the one this screen can actually fix.
  const primary = roster.gateways.find((g) => g.id === "openclaw" && g.enabled) ?? roster.gateways.find((g) => g.enabled);
  const status = primary?.status ?? null;
  const others = roster.gateways.filter((g) => g.enabled && g.id !== primary?.id);
  const [report, setReport] = useState<DoctorReport | null>(null);
  const [checking, setChecking] = useState(false);
  const phase = status?.phase ?? "starting";

  const check = useCallback(async () => {
    setChecking(true);
    try {
      setReport(await core.doctor.check());
    } catch {
      /* the status card still explains the basics */
    } finally {
      setChecking(false);
    }
  }, []);

  // Diagnose once we know we're not just mid-connect.
  useEffect(() => {
    if (phase === "starting" || phase === "connecting") {
      const t = setTimeout(check, 6000);
      return () => clearTimeout(t);
    }
    check();
  }, [phase, check]);

  return (
    <div className="flex h-full w-full overflow-y-auto">
      <div className="m-auto w-full max-w-[540px] px-8 py-10">
        <div className="flex flex-col items-center text-center">
          <GuyMark size={104} mood={phase === "offline" || phase === "error" ? "sleepy" : phase === "no-config" ? "wave" : "happy"} />
          <h1 className="mt-3 font-display text-[22px] font-semibold tracking-tight">{headline(status)}</h1>
          <p className="mt-1.5 max-w-md text-[13px] text-[var(--color-ink-2)]">{subline(status)}</p>
        </div>

        <div className="mt-7 space-y-3">
          {phase === "pairing" && (
            <Card>
              <PairingFix status={status!} />
            </Card>
          )}
          {phase === "auth-failed" && (
            <Card>
              <CredentialFix status={status!} />
            </Card>
          )}
          {needsProxyFix(status) && (
            <Card>
              <ProxyFix status={status!} />
            </Card>
          )}
          {report?.issues
            .filter((i) => i.fix !== "pairing" && i.fix !== "credential")
            .map((issue) => <IssueCard key={issue.id} issue={issue} report={report} onDone={check} />)}
          {report && report.issues.length === 0 && phase !== "pairing" && phase !== "auth-failed" && phase !== "connecting" && phase !== "starting" && !needsProxyFix(status) && (
            <p className="text-center text-[12.5px] text-[var(--color-ink-3)]">
              Nothing obviously broken — littleguys keeps retrying{status?.message ? ` (${status.message})` : ""}.
            </p>
          )}
        </div>

        {others.length > 0 && (
          <div className="mt-5 space-y-2">
            <p className="text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">Your other gateways</p>
            {others.map((g) => (
              <Card key={g.id}>
                <div className="flex items-center justify-between gap-2">
                  <span className="font-medium">{g.name}</span>
                  <span className="text-[12px] text-[var(--color-ink-3)]">{statusLabel(g.status)}</span>
                </div>
                {g.status.phase === "pairing" && (
                  <div className="mt-2">
                    <PairingFix status={g.status} compact />
                  </div>
                )}
                {g.status.phase === "auth-failed" && (
                  <div className="mt-2">
                    <CredentialFix status={g.status} />
                  </div>
                )}
                {needsProxyFix(g.status) && (
                  <div className="mt-2">
                    <ProxyFix status={g.status} compact />
                  </div>
                )}
              </Card>
            ))}
          </div>
        )}

        <div className="mt-6 flex items-center justify-center gap-2">
          <Button onClick={() => core.reconnect()}>
            <RotateCw size={13} /> Retry now
          </Button>
          <Button variant="ghost" onClick={check} disabled={checking}>
            {checking ? <Spinner size={12} /> : <Stethoscope size={13} />} Check again
          </Button>
          <Button variant="ghost" onClick={() => setAddGatewayOpen(true)}>
            <Plus size={13} /> Add a gateway
          </Button>
        </div>
        {report && <Details report={report} />}
      </div>
    </div>
  );
}

function headline(status: ConnStatus | null): string {
  switch (status?.phase) {
    case "pairing":
      return "One quick approval";
    case "auth-failed":
      return "littleguys can't sign in";
    case "offline":
      return status.mode === "remote" ? "Your gateway is unreachable" : "Your gateway is asleep";
    case "no-config":
      return "Let's set up your little guys";
    case "error":
      return needsProxyFix(status) ? "One setting on your gateway" : "Something's off";
    default:
      return "Waking up…";
  }
}

function subline(status: ConnStatus | null): string {
  switch (status?.phase) {
    case "pairing":
      return `For security, ${hostOf(status.url)} needs to approve littleguys once. After that it just works.`;
    case "auth-failed":
      return status.credentialIssue ?? status.message ?? "The gateway rejected littleguys's credentials.";
    case "offline":
      return status.mode === "remote"
        ? `Can't reach ${hostOf(status.url)}. Is it powered on and on your network/tailnet?`
        : "OpenClaw's gateway isn't running on this Mac.";
    case "no-config":
      return "littleguys runs on OpenClaw. It'll install and set it up for you — no terminal needed.";
    case "error":
      return needsProxyFix(status) ? `${hostOf(status.url)} won't take connections through its Tailscale Serve route yet.` : (status.message ?? "");
    default:
      return status?.url ? `Connecting to ${hostOf(status.url)}…` : "Connecting to OpenClaw…";
  }
}

function Card({ children }: { children: React.ReactNode }) {
  return <div className="rounded-2xl border border-[var(--color-line)] bg-[var(--color-pane-solid)] p-4 shadow-[0_1px_3px_rgba(0,0,0,0.04)]">{children}</div>;
}

function useProgress() {
  const [lines, setLines] = useState<string[]>([]);
  useEffect(() => {
    const un = listen<{ step: string; line: string }>("lg:doctor-progress", (p) =>
      setLines((l) => [...l.slice(-200), p.line]),
    );
    return () => {
      un.then((u) => u());
    };
  }, []);
  return [lines, setLines] as const;
}

function IssueCard({ issue, report, onDone }: { issue: DoctorIssue; report: DoctorReport; onDone: () => void }) {
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lines, setLines] = useProgress();
  const logRef = useRef<HTMLPreElement>(null);
  useEffect(() => {
    logRef.current?.scrollTo({ top: logRef.current.scrollHeight });
  }, [lines]);

  const run = async (fn: () => Promise<void>) => {
    setRunning(true);
    setError(null);
    setLines([]);
    try {
      await fn();
      onDone();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setRunning(false);
    }
  };

  return (
    <Card>
      <p className="font-medium">{issue.title}</p>
      <p className="mt-0.5 text-[12.5px] text-[var(--color-ink-2)]">{issue.detail}</p>
      {issue.fix === "onboard" ? (
        <OnboardForm running={running} onSubmit={(choice, key, name) => run(() => core.doctor.onboard(choice, key, name))} />
      ) : (
        <div className="mt-3 flex items-center gap-2">
          {issue.fix === "install" && (
            <Button variant="primary" disabled={running} onClick={() => run(() => core.doctor.install())}>
              {running && <Spinner size={12} />} Install OpenClaw
            </Button>
          )}
          {issue.fix === "update" && (
            <>
              <Button variant="primary" disabled={running} onClick={() => run(() => core.doctor.update())}>
                {running && <Spinner size={12} />} Update OpenClaw
              </Button>
              {error && (
                <Button disabled={running} onClick={() => run(() => core.doctor.install("latest"))}>
                  Install a fresh copy instead
                </Button>
              )}
            </>
          )}
          {issue.fix === "start" && (
            <Button variant="primary" disabled={running} onClick={() => run(() => core.doctor.startGateway())}>
              {running && <Spinner size={12} />} {report.serviceInstalled ? "Restart gateway" : "Start gateway"}
            </Button>
          )}
        </div>
      )}
      {(running || lines.length > 0) && (
        <pre ref={logRef} className="selectable mt-3 max-h-36 overflow-auto rounded-lg bg-[var(--color-chip)] p-2.5 font-mono text-[11px] text-[var(--color-ink-2)]">
          {lines.join("\n") || "Starting…"}
        </pre>
      )}
      {error && <p className="mt-2 text-[12px] text-[var(--color-bad)]">{error}</p>}
    </Card>
  );
}

const PROVIDERS = [
  { id: "apiKey", label: "Anthropic (Claude) API key", needsKey: true, placeholder: "sk-ant-…" },
  { id: "anthropic-cli", label: "Claude subscription (uses Claude Code login)", needsKey: false },
  { id: "openai-api-key", label: "OpenAI API key", needsKey: true, placeholder: "sk-…" },
  { id: "openrouter-api-key", label: "OpenRouter API key", needsKey: true, placeholder: "sk-or-…" },
  { id: "gemini-api-key", label: "Google Gemini API key", needsKey: true, placeholder: "AIza…" },
  { id: "ollama", label: "Ollama (local models)", needsKey: false },
];

function OnboardForm({ running, onSubmit }: { running: boolean; onSubmit: (choice: string, key: string | null, name: string | null) => void }) {
  const [choice, setChoice] = useState(PROVIDERS[0].id);
  const [key, setKey] = useState("");
  const [name, setName] = useState("");
  const provider = PROVIDERS.find((p) => p.id === choice)!;
  return (
    <div className="mt-3 space-y-3">
      <Field label="AI provider">
        <select className={inputClass} value={choice} onChange={(e) => setChoice(e.target.value)} disabled={running}>
          {PROVIDERS.map((p) => (
            <option key={p.id} value={p.id}>
              {p.label}
            </option>
          ))}
        </select>
      </Field>
      {provider.needsKey && (
        <Field label="API key" hint="Stored in OpenClaw's config on this Mac.">
          <input type="password" className={inputClass} value={key} onChange={(e) => setKey(e.target.value)} placeholder={provider.placeholder} disabled={running} />
        </Field>
      )}
      <Field label="Name your first little guy">
        <input className={inputClass} value={name} onChange={(e) => setName(e.target.value)} placeholder="Fox" disabled={running} />
      </Field>
      <Button
        variant="primary"
        disabled={running || (provider.needsKey && !key.trim())}
        onClick={() => onSubmit(choice, provider.needsKey ? key.trim() : null, name.trim() || null)}
      >
        {running && <Spinner size={12} />} Set it up
      </Button>
    </div>
  );
}

function Details({ report }: { report: DoctorReport }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="mt-8">
      <button onClick={() => setOpen(!open)} className="mx-auto flex items-center gap-1 text-[11.5px] text-[var(--color-ink-3)] hover:text-[var(--color-ink-2)]">
        <ChevronRight size={12} className={`transition ${open ? "rotate-90" : ""}`} /> Details
      </button>
      {open && (
        <div className="selectable mt-2 space-y-1 rounded-xl bg-[var(--color-chip)] p-3 font-mono text-[11px] text-[var(--color-ink-2)]">
          <div>mode: {report.mode}</div>
          <div>gateway: {report.gatewayUrl}</div>
          <div>config: {report.configPath} {report.configExists ? `(written by ${report.configVersion ?? "?"})` : "(missing)"}</div>
          <div>cli: {report.cliPath ?? "not found"} {report.cliVersion ? `v${report.cliVersion}` : ""}</div>
          {report.cliProblem && <div>cli problem: {report.cliProblem}</div>}
          {report.mode === "local" && (
            <div>
              service: {report.serviceInstalled ? "installed" : "not installed"}
              {report.service.loaded ? `, ${report.service.running ? `running (pid ${report.service.pid})` : `stopped (last exit ${report.service.lastExitCode ?? "?"})`}` : ""}
            </div>
          )}
          {report.logTail && <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap">{report.logTail}</pre>}
        </div>
      )}
    </div>
  );
}
