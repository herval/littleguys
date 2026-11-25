import { Terminal, Waypoints } from "lucide-react";
import { useEffect, useState } from "react";
import claudeFace from "../../src-tauri/assets/claude-code.svg";
import { core } from "../lib/ipc";
import { COMPUTER } from "../lib/platform";
import type { ClaudeCodeProbe, CodexProbe } from "../lib/types";
import { type AddBotTab, useApp } from "../stores/app";
import { Button } from "../ui/kit";

/**
 * The agents littleguys connects to besides OpenClaw, each opening "Add a bot"
 * on its tab, which installs Claude Code or Codex when they're missing: on the
 * first-run screen, and in Settings › Agents. Those two drop out once added, since
 * there's one of each.
 */
export function AgentsToAdd() {
  const openAdd = useApp((s) => s.setAddGatewayOpen);
  const hasClaude = useApp((s) => s.roster.gateways.some((g) => g.status.kind === "claudecode"));
  const hasCodex = useApp((s) => s.roster.gateways.some((g) => g.status.kind === "codex"));
  const [claude, setClaude] = useState<ClaudeCodeProbe | null>(null);
  const [codex, setCodex] = useState<CodexProbe | null>(null);
  useEffect(() => {
    core.bots
      .claudeDetect()
      .then(setClaude)
      .catch(() => setClaude({ installed: false }));
    core.bots
      .codexDetect()
      .then(setCodex)
      .catch(() => setCodex({ installed: false }));
  }, []);
  // What's missing, Add a bot installs.
  const found = (probe: { installed: boolean } | null) =>
    probe?.installed ? `Found on this ${COMPUTER}. ` : probe ? `Not on this ${COMPUTER} yet: littleguys installs it. ` : "";
  const rows: Array<{ tab: AddBotTab; icon: React.ReactNode; title: string; detail: string }> = [
    ...(hasClaude
      ? []
      : [
          {
            tab: "claude" as const,
            icon: <img src={claudeFace} alt="" className="h-8 w-8 rounded-lg" />,
            title: "Claude Code",
            detail: `${found(claude)}Every session, terminal ones too, as a chat you can answer.`,
          },
        ]),
    ...(hasCodex
      ? []
      : [
          {
            tab: "codex" as const,
            icon: <span className="text-[16px]">🌀</span>,
            title: "Codex",
            detail: `${found(codex)}Every session, terminal ones too, as a chat you can answer.`,
          },
        ]),
    {
      tab: "acp",
      icon: <Terminal size={16} />,
      title: "Gemini CLI and more",
      detail: "Any agent from the ACP registry: littleguys starts it for you.",
    },
    {
      tab: "hermes",
      icon: <Waypoints size={16} />,
      title: "Hermes",
      detail: `Your Hermes Agent profiles, on this ${COMPUTER} or a server.`,
    },
  ];
  return (
    <div className="divide-y divide-[var(--color-line)]">
      {rows.map((r) => (
        <div key={r.tab} className="flex items-center gap-3 py-2.5">
          <span className="grid h-8 w-8 shrink-0 place-items-center rounded-lg bg-[var(--color-chip)] text-[var(--color-ink-2)]">{r.icon}</span>
          <div className="min-w-0 flex-1">
            <p className="font-medium">{r.title}</p>
            <p className="text-[12px] text-[var(--color-ink-3)]">{r.detail}</p>
          </div>
          <Button size="sm" onClick={() => openAdd(true, r.tab)}>
            Add
          </Button>
        </div>
      ))}
    </div>
  );
}
