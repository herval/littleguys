// "Add a bot" tabs for runtimes other than OpenClaw: Hermes and ACP agents.
import { Check, ChevronRight, Folder, Laptop, Network, RefreshCw, Terminal } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { core, errorMessage, isTauri } from "../lib/ipc";
import type { HermesProbe, RegistryAgent } from "../lib/types";
import { useApp } from "../stores/app";
import { Button, Field, inputBase, inputClass, Spinner } from "../ui/kit";

const sectionTitle = "mb-2 block text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]";

/** For addresses, commands and names the server matches exactly (Hermes profiles are case-sensitive). */
const verbatim = { autoCapitalize: "off", autoCorrect: "off", spellCheck: false } as const;

// ---- Hermes ------------------------------------------------------------------

export function HermesTab({ onAdded }: { onAdded: (id: string) => void }) {
  const [found, setFound] = useState<HermesProbe[] | null>(null);
  const [searching, setSearching] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [keyFor, setKeyFor] = useState<string | null>(null);
  const [mode, setMode] = useState<"local" | "api">("api");
  // Another profile usually lives on a server that's already added: start from its address.
  const knownServer = useApp((s) => {
    const g = s.roster.gateways.find((g) => g.status.kind === "hermes" && g.status.mode === "remote" && g.status.url);
    return g?.status.url?.replace(/\/p\/.*$/, "") ?? "";
  });
  const [url, setUrl] = useState(knownServer);
  const [apiKey, setApiKey] = useState("");
  const [profile, setProfile] = useState("");

  const search = useCallback(async () => {
    setSearching(true);
    try {
      setFound(await core.bots.hermesDetect());
    } catch {
      setFound([]);
    } finally {
      setSearching(false);
    }
  }, []);
  useEffect(() => {
    search();
  }, [search]);

  const add = async (m: "local" | "api", u: string | null, key?: string, prof?: string) => {
    setBusy(true);
    setError(null);
    try {
      const entry = await core.bots.addHermes({ mode: m, url: u, profile: prof || null }, undefined, key || undefined);
      onAdded(entry.id);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      <div className="mb-2 flex items-center justify-between">
        <span className={sectionTitle.replace("mb-2 block ", "")}>Found</span>
        <button
          onClick={search}
          disabled={searching}
          className="flex items-center gap-1 text-[11.5px] text-[var(--color-ink-3)] hover:text-[var(--color-ink)] disabled:opacity-50"
        >
          <RefreshCw size={11} className={searching ? "animate-spin" : ""} /> Search again
        </button>
      </div>
      <div className="rounded-xl border border-[var(--color-line)]">
        {found === null ? (
          <div className="flex items-center gap-2 px-3.5 py-4 text-[12.5px] text-[var(--color-ink-3)]">
            <Spinner /> Looking for Hermes on this Mac and your tailnet…
          </div>
        ) : found.length === 0 ? (
          <p className="px-3.5 py-4 text-[12.5px] text-[var(--color-ink-3)]">
            No Hermes found. On this Mac, start it with <span className="font-mono">hermes serve</span>. For another machine, turn on its API
            server (<span className="font-mono">API_SERVER_ENABLED=true</span> and an <span className="font-mono">API_SERVER_KEY</span> in{" "}
            <span className="font-mono">~/.hermes/.env</span>) and add it below.
          </p>
        ) : (
          found.map((h, i) => (
            <div key={h.url} className={`px-3.5 py-2.5 ${i ? "border-t border-[var(--color-line)]" : ""}`}>
              <div className="flex items-center gap-3">
                <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-[var(--color-chip)] text-[var(--color-ink-2)]">
                  {h.mode === "local" ? <Laptop size={15} /> : <Network size={15} />}
                </span>
                <div className="min-w-0 flex-1">
                  <p className="font-medium">
                    {h.mode === "local" ? "Hermes on this Mac" : `Hermes API server`}
                    {h.version && <span className="ml-1.5 text-[11px] font-normal text-[var(--color-ink-3)]">v{h.version}</span>}
                  </p>
                  <p className="truncate font-mono text-[11px] text-[var(--color-ink-3)]">{h.url}</p>
                </div>
                <Button
                  size="sm"
                  variant="primary"
                  disabled={busy}
                  onClick={() => (h.mode === "api" ? setKeyFor(keyFor === h.url ? null : h.url) : add("local", h.url))}
                >
                  Add
                </Button>
              </div>
              {keyFor === h.url && (
                <div className="mt-2 flex gap-2 pl-11">
                  <input
                    type="password"
                    className={inputClass}
                    value={apiKey}
                    onChange={(e) => setApiKey(e.target.value)}
                    placeholder="API_SERVER_KEY from that machine's ~/.hermes/.env"
                  />
                  <Button variant="primary" disabled={busy || !apiKey.trim()} onClick={() => add("api", h.url, apiKey.trim())}>
                    Connect
                  </Button>
                </div>
              )}
            </div>
          ))
        )}
      </div>

      <div className="mt-5">
        <span className={sectionTitle}>Or enter one</span>
        <div className="space-y-2">
          <div className="flex gap-2">
            <select className={`${inputBase} w-auto`} value={mode} onChange={(e) => setMode(e.target.value as "local" | "api")}>
              <option value="api">Remote API server</option>
              <option value="local">This Mac (hermes serve)</option>
            </select>
            <input
              className={inputClass}
              {...verbatim}
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              placeholder={mode === "api" ? "http://studio.your-tailnet.ts.net:8642" : "http://127.0.0.1:9119 (optional)"}
            />
          </div>
          <div className="flex gap-2">
            {mode === "api" && (
              <input type="password" className={inputClass} value={apiKey} onChange={(e) => setApiKey(e.target.value)} placeholder="API key" />
            )}
            <input className={`${inputBase} w-40`} {...verbatim} value={profile} onChange={(e) => setProfile(e.target.value)} placeholder="Profile (optional)" />
          </div>
          <Button
            variant="primary"
            disabled={busy || (mode === "api" && (!url.trim() || !apiKey.trim()))}
            onClick={() => add(mode, url.trim() || null, mode === "api" ? apiKey.trim() : undefined, profile.trim())}
          >
            {busy && <Spinner size={11} />} Add Hermes
          </Button>
        </div>
      </div>
      {error && <p className="mt-3 text-[12.5px] text-[var(--color-bad)]">{error}</p>}
      <p className="mt-4 text-[11.5px] text-[var(--color-ink-3)]">Each Hermes profile shows up as its own little guy.</p>
    </div>
  );
}

// ---- ACP agents ------------------------------------------------------------------

const AGENT_EMOJI: Record<string, string> = {
  claude: "✳️",
  codex: "🌀",
  gemini: "✦",
  goose: "🪿",
  opencode: "🧩",
  cursor: "🖱️",
  copilot: "🧑‍✈️",
  qwen: "🐉",
  kimi: "🌙",
  zeroclaw: "🦀",
  letta: "🧠",
};

function emojiFor(id: string): string {
  const hit = Object.keys(AGENT_EMOJI).find((k) => id.toLowerCase().includes(k));
  return hit ? AGENT_EMOJI[hit] : "🤖";
}

/** "Claude Code" → "Claude", "Gemini CLI" → "Gemini". */
function friendlyName(name: string): string {
  return name.replace(/\s+(code|cli|agent|acp)$/i, "").trim() || name;
}

/** Split a command line, honoring simple quotes. */
export function splitCommand(line: string): string[] {
  const out: string[] = [];
  const re = /"([^"]*)"|'([^']*)'|(\S+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(line))) out.push(m[1] ?? m[2] ?? m[3]);
  return out;
}

export function AcpTab({ onAdded }: { onAdded: (id: string) => void }) {
  const [agents, setAgents] = useState<RegistryAgent[] | null>(null);
  const [registryError, setRegistryError] = useState<string | null>(null);
  const [picked, setPicked] = useState<RegistryAgent | "custom" | null>(null);

  useEffect(() => {
    core.bots
      .acpRegistry()
      .then(setAgents)
      .catch((e) => {
        setAgents([]);
        setRegistryError(errorMessage(e));
      });
  }, []);

  if (picked) return <AcpForm agent={picked} onBack={() => setPicked(null)} onAdded={onAdded} />;

  return (
    <div>
      <span className={sectionTitle}>Pick an agent</span>
      <div className="max-h-[300px] overflow-y-auto rounded-xl border border-[var(--color-line)]">
        {agents === null ? (
          <div className="flex items-center gap-2 px-3.5 py-4 text-[12.5px] text-[var(--color-ink-3)]">
            <Spinner /> Loading the ACP registry…
          </div>
        ) : (
          <>
            {agents.map((a, i) => (
              <button
                key={a.id}
                onClick={() => setPicked(a)}
                className={`flex w-full items-center gap-3 px-3.5 py-2.5 text-left transition hover:bg-[var(--color-chip)] ${
                  i ? "border-t border-[var(--color-line)]" : ""
                }`}
              >
                <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-[var(--color-chip)] text-[16px]">
                  {emojiFor(a.id)}
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block font-medium">
                    {a.name} <span className="text-[11px] font-normal text-[var(--color-ink-3)]">v{a.version}</span>
                  </span>
                  <span className="block truncate text-[11.5px] text-[var(--color-ink-3)]">{a.description}</span>
                </span>
                <ChevronRight size={14} className="text-[var(--color-ink-3)]" />
              </button>
            ))}
            {agents.length === 0 && (
              <p className="px-3.5 py-3 text-[12.5px] text-[var(--color-ink-3)]">
                {registryError ? `Couldn't load the ACP registry (${registryError}).` : "The registry is empty."}
              </p>
            )}
          </>
        )}
      </div>
      <button
        onClick={() => setPicked("custom")}
        className="mt-3 flex items-center gap-1.5 text-[12.5px] text-[var(--color-accent-strong)] hover:underline"
      >
        <Terminal size={13} /> Something else — a command or an ACP server URL
      </button>
      <p className="mt-4 text-[11.5px] text-[var(--color-ink-3)]">
        Agents run on this Mac while littleguys is open and use your existing login for that tool. littleguys runs their routines.
      </p>
    </div>
  );
}

function AcpForm({ agent, onBack, onAdded }: { agent: RegistryAgent | "custom"; onBack: () => void; onAdded: (id: string) => void }) {
  const custom = agent === "custom";
  const [name, setName] = useState(custom ? "" : friendlyName(agent.name));
  const [emoji, setEmoji] = useState(custom ? "🤖" : emojiFor(agent.id));
  const [cwd, setCwd] = useState("~");
  const [how, setHow] = useState<"command" | "url">("command");
  const [commandLine, setCommandLine] = useState("");
  const [url, setUrl] = useState("");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const pickFolder = async () => {
    if (!isTauri) return;
    const { open } = await import("@tauri-apps/plugin-dialog");
    const dir = await open({ directory: true, multiple: false, title: "Where should this agent work?" });
    if (typeof dir === "string") setCwd(dir.replace(/^\/Users\/[^/]+/, "~"));
  };

  const add = async () => {
    if (!name.trim()) return setError("Give it a name.");
    setBusy(true);
    setError(null);
    try {
      let launch: { command: string | null; args: string[]; env: Record<string, string> } = { command: null, args: [], env: {} };
      if (!custom) launch = { command: agent.command, args: agent.args, env: agent.env };
      else if (how === "command") {
        const [command, ...args] = splitCommand(commandLine.trim());
        if (!command) throw new Error("Enter the command that starts the agent in ACP mode.");
        launch = { command, args, env: {} };
      }
      const entry = await core.bots.addAcp(
        {
          transport: custom && how === "url" ? "websocket" : "stdio",
          command: launch.command,
          args: launch.args,
          env: launch.env,
          url: custom && how === "url" ? url.trim() : null,
          cwd: cwd.trim() || "~",
          agentName: name.trim(),
          emoji,
          registryId: custom ? null : agent.id,
        },
        custom ? name.trim() : agent.name,
        custom && how === "url" && token.trim() ? token.trim() : undefined,
      );
      onAdded(entry.id);
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  return (
    <div className="space-y-3.5">
      <button onClick={onBack} className="text-[12px] text-[var(--color-ink-3)] hover:text-[var(--color-ink)]">
        ← All agents
      </button>
      {!custom && (
        <div className="flex items-center gap-3 rounded-xl bg-[var(--color-chip)] px-3.5 py-2.5">
          <span className="text-[20px]">{emojiFor(agent.id)}</span>
          <div className="min-w-0">
            <p className="font-medium">{agent.name}</p>
            <p className="truncate font-mono text-[11px] text-[var(--color-ink-3)]">{[agent.command, ...agent.args].join(" ")}</p>
          </div>
        </div>
      )}
      {custom && (
        <Field label="How does it run?">
          <div className="flex gap-2">
            <select className={`${inputBase} w-auto`} value={how} onChange={(e) => setHow(e.target.value as "command" | "url")}>
              <option value="command">Command on this Mac</option>
              <option value="url">ACP server (WebSocket)</option>
            </select>
            {how === "command" ? (
              <input className={`${inputClass} font-mono`} {...verbatim} value={commandLine} onChange={(e) => setCommandLine(e.target.value)} placeholder="hermes acp" />
            ) : (
              <input className={`${inputClass} font-mono`} {...verbatim} value={url} onChange={(e) => setUrl(e.target.value)} placeholder="ws://studio.local:3284/acp" />
            )}
          </div>
          {how === "url" && (
            <input type="password" className={`${inputClass} mt-2`} value={token} onChange={(e) => setToken(e.target.value)} placeholder="Token (if the server needs one)" />
          )}
        </Field>
      )}
      <div className="flex gap-3">
        <Field label="Name">
          <input className={inputClass} value={name} onChange={(e) => setName(e.target.value)} placeholder="Claude" />
        </Field>
        <Field label="Face">
          <input className={`${inputBase} w-16 text-center text-[16px]`} value={emoji} onChange={(e) => setEmoji(e.target.value.trim())} />
        </Field>
      </div>
      {!(custom && how === "url") && (
        <Field label="Works in" hint="The folder this agent reads and edits. You can open it from the sidebar.">
          <div className="flex gap-2">
            <input className={`${inputClass} font-mono`} {...verbatim} value={cwd} onChange={(e) => setCwd(e.target.value)} />
            {isTauri && (
              <Button onClick={pickFolder}>
                <Folder size={13} /> Choose…
              </Button>
            )}
          </div>
        </Field>
      )}
      {error && <p className="text-[12.5px] text-[var(--color-bad)]">{error}</p>}
      <Button variant="primary" disabled={busy} onClick={add}>
        {busy ? <Spinner size={11} /> : <Check size={13} />} Add {name.trim() || "agent"}
      </Button>
    </div>
  );
}
