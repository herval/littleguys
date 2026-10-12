import { ShieldAlert } from "lucide-react";
import { useMemo, useState } from "react";
import { approvals as approvalsApi } from "../lib/gateway";
import { gatewayOf, hasMainChat } from "../lib/guys";
import { core, errorMessage } from "../lib/ipc";
import type { ExecApproval, Guy } from "../lib/types";
import { agentIdFromKey, useApp } from "../stores/app";
import { Button } from "../ui/kit";

type Decision = "allow-once" | "allow-always" | "deny";

interface Presentation {
  commandText?: string;
  warningText?: string | null;
  allowedDecisions?: string[];
}

function approvalId(a: ExecApproval): string | undefined {
  return a.id ?? a.approvalId ?? a.request?.id;
}

function approvalAgent(a: ExecApproval): string | null {
  return a.agentId ?? a.request?.agentId ?? agentIdFromKey(a.sessionKey ?? a.request?.sessionKey ?? "");
}

/**
 * Pending "can I run this?" requests from the selected guy, answered inline.
 * When the guy's threads are separate sessions (ACP, Claude Code), each shows only its own.
 */
export function Approvals({ guy, sessionKey }: { guy: Guy; sessionKey: string }) {
  const all = useApp((s) => gatewayOf(s.roster, guy.gatewayId)?.approvals);
  const perSession = useApp((s) => !hasMainChat(s.roster, guy.gatewayId));
  const approvals = useMemo(
    () =>
      (all ?? []).filter(
        (a) => approvalAgent(a) === guy.id && (!perSession || (a.sessionKey ?? a.request?.sessionKey) === sessionKey),
      ),
    [all, guy.id, perSession, sessionKey],
  );
  if (approvals.length === 0) return null;
  return (
    <div className="mx-auto w-full max-w-[900px] space-y-2 px-4 pb-1">
      {approvals.map((a) => (
        <ApprovalCard key={approvalId(a)} gatewayId={guy.gatewayId} approval={a} />
      ))}
    </div>
  );
}

function ApprovalCard({ gatewayId, approval }: { gatewayId: string; approval: ExecApproval }) {
  const [busy, setBusy] = useState<Decision | null>(null);
  const [error, setError] = useState<string | null>(null);
  const presentation = (approval.presentation ?? {}) as Presentation;
  const command = presentation.commandText ?? approval.request?.command ?? approval.command ?? "a command";
  const cwd = approval.request?.cwd;
  const allowed = presentation.allowedDecisions ?? ["allow-once", "allow-always", "deny"];
  const id = approvalId(approval);

  const decide = async (decision: Decision) => {
    if (!id) return;
    setBusy(decision);
    setError(null);
    try {
      await approvalsApi.resolve(gatewayId, id, decision);
      core.refreshRoster(gatewayId).catch(() => {});
    } catch (e) {
      setError(errorMessage(e));
      setBusy(null);
    }
  };

  return (
    <div className="animate-pop rounded-2xl border border-[var(--color-warn)]/40 bg-[var(--color-warn)]/8 p-3">
      <div className="flex items-start gap-2.5">
        <ShieldAlert size={18} className="mt-0.5 shrink-0 text-[var(--color-warn)]" />
        <div className="min-w-0 flex-1">
          <p className="text-[12.5px] font-medium">Can I run this?</p>
          <pre className="selectable mt-1 max-h-28 overflow-auto whitespace-pre-wrap rounded-lg bg-[var(--color-chip)] px-2.5 py-1.5 font-mono text-[11.5px]">
            {command}
          </pre>
          {cwd && <p className="mt-1 truncate text-[11px] text-[var(--color-ink-3)]">in {cwd.replace(/^\/Users\/[^/]+/, "~")}</p>}
          {presentation.warningText && <p className="mt-1 text-[11.5px] text-[var(--color-bad)]">{presentation.warningText}</p>}
          {error && <p className="mt-1 text-[11.5px] text-[var(--color-bad)]">{error}</p>}
          <div className="mt-2 flex flex-wrap gap-1.5">
            {allowed.includes("allow-once") && (
              <Button size="sm" variant="primary" disabled={busy !== null} onClick={() => decide("allow-once")}>
                Allow once
              </Button>
            )}
            {allowed.includes("allow-always") && (
              <Button size="sm" disabled={busy !== null} onClick={() => decide("allow-always")}>
                Always allow
              </Button>
            )}
            {allowed.includes("deny") && (
              <Button size="sm" variant="danger" disabled={busy !== null} onClick={() => decide("deny")}>
                Deny
              </Button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
