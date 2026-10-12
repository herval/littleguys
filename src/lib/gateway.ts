// Typed wrappers for the gateway methods littleguys uses. Every call names the
// gateway it goes to (`gw` = gateway id).
import { gwCall } from "./ipc";
import type {
  AgentSummary,
  Attachment,
  ChatMessage,
  CommandEntry,
  CronJob,
  CronRunLogEntry,
  McpServer,
  ModelChoice,
  PastSession,
  PluginEntry,
  SessionRow,
  SkillEntry,
  ToolGroup,
  WorkspaceFile,
  WorkspaceListing,
} from "./types";

export function newIdempotencyKey(): string {
  return crypto.randomUUID();
}

export const agents = {
  list: (gw: string) => gwCall<{ defaultId: string; agents: AgentSummary[] }>(gw, "agents.list"),
  create: (gw: string, p: { name: string; model?: string; emoji?: string; avatar?: string }) =>
    gwCall<{ ok: boolean; agentId: string; name: string; workspace: string }>(gw, "agents.create", p),
  update: (gw: string, p: { agentId: string; name?: string; model?: string | null; emoji?: string; avatar?: string }) =>
    gwCall<{ ok: boolean }>(gw, "agents.update", p),
  remove: (gw: string, agentId: string, deleteFiles: boolean) =>
    gwCall<{ ok: boolean }>(gw, "agents.delete", { agentId, deleteFiles }),
  getFile: (gw: string, agentId: string, name: string) =>
    gwCall<{ agentId: string; workspace: string; file: { name: string; content?: string; hash?: string; missing?: boolean } }>(
      gw,
      "agents.files.get",
      { agentId, name },
    ),
  setFile: (gw: string, agentId: string, name: string, content: string, expectedHash?: string) =>
    gwCall<{ ok: boolean }>(gw, "agents.files.set", { agentId, name, content, ...(expectedHash ? { expectedHash } : {}) }),
};

export const sessions = {
  create: (gw: string, agentId: string, label?: string) =>
    gwCall<{ ok: boolean; key: string; sessionId?: string }>(gw, "sessions.create", {
      agentId,
      idempotencyKey: newIdempotencyKey(),
      ...(label ? { label } : {}),
    }),
  patch: (
    gw: string,
    key: string,
    patch: { label?: string | null; pinned?: boolean; archived?: boolean; unread?: boolean },
    expectedSessionId?: string,
  ) => gwCall<SessionRow>(gw, "sessions.patch", { key, ...patch, ...(expectedSessionId ? { expectedSessionId } : {}) }),
  remove: (gw: string, key: string, expectedSessionId?: string) =>
    gwCall<{ ok: boolean }>(gw, "sessions.delete", {
      key,
      archivedOnly: true,
      deleteTranscript: true,
      ...(expectedSessionId ? { expectedSessionId } : {}),
    }),
  /** A guy's archived threads; the roster only has the ones that aren't. */
  archived: (gw: string, agentId: string) =>
    gwCall<{ sessions: SessionRow[] }>(gw, "sessions.list", {
      agentId,
      archived: true,
      limit: 200,
      includeDerivedTitles: true,
      includeLastMessage: true,
    }),
  subscribeMessages: (gw: string, key: string) => gwCall<{ subscribed: boolean }>(gw, "sessions.messages.subscribe", { key }),
  /** Every session the agent can resume, from every folder, newest first (ACP). */
  past: (gw: string) => gwCall<{ sessions: PastSession[] }>(gw, "sessions.past", {}, 60_000),
  /** Make a past session a thread that works in its own folder; resolves to its key. */
  open: (gw: string, s: Pick<PastSession, "sessionId" | "cwd" | "title" | "updatedAt">) =>
    gwCall<{ key: string }>(gw, "sessions.open", s),
};

export interface ChatHistory {
  sessionKey: string;
  sessionId?: string;
  messages: ChatMessage[];
  hasMore?: boolean;
  nextOffset?: number;
  totalMessages?: number;
  sessionInfo?: SessionRow;
  inFlightRun?: { runId: string; message?: ChatMessage } | null;
}

export const chat = {
  history: (gw: string, sessionKey: string, limit = 200) =>
    gwCall<ChatHistory>(gw, "chat.history", { sessionKey, limit, maxChars: 40_000 }, 60_000),
  /** `cwd`: folder for a thread's first message (ACP agents). */
  send: (gw: string, sessionKey: string, message: string, attachments: Attachment[] = [], cwd?: string) =>
    gwCall<{ runId: string; status: string }>(
      gw,
      "chat.send",
      {
        sessionKey,
        message,
        idempotencyKey: newIdempotencyKey(),
        ...(cwd ? { cwd } : {}),
        ...(attachments.length
          ? {
              attachments: attachments.map(({ type, mimeType, fileName, content, sizeBytes }) => ({
                type,
                mimeType,
                fileName,
                content,
                sizeBytes,
              })),
            }
          : {}),
      },
      120_000,
    ),
  abort: (gw: string, sessionKey: string, runId?: string) =>
    gwCall(gw, "chat.abort", { sessionKey, ...(runId ? { runId } : {}) }),
};

export const artifacts = {
  download: (gw: string, sessionKey: string, artifactId: string) =>
    gwCall<{ url?: string; encoding?: string; data?: string; artifact?: { mimeType?: string } }>(gw, "artifacts.download", {
      sessionKey,
      artifactId,
    }),
};

export const models = {
  list: (gw: string, agentId?: string) =>
    gwCall<{ models: ModelChoice[] }>(gw, "models.list", { view: "configured", ...(agentId ? { agentId } : {}) }),
};

export const cron = {
  list: (gw: string, agentId?: string) =>
    gwCall<{ jobs: CronJob[]; total: number }>(gw, "cron.list", {
      includeDisabled: true,
      limit: 200,
      ...(agentId ? { agentId } : {}),
    }),
  add: (gw: string, job: Record<string, unknown>) => gwCall<CronJob>(gw, "cron.add", job),
  update: (gw: string, id: string, patch: Record<string, unknown>) => gwCall<CronJob>(gw, "cron.update", { id, patch }),
  remove: (gw: string, id: string) => gwCall(gw, "cron.remove", { id }),
  run: (gw: string, id: string) => gwCall<{ ok: boolean; runId?: string }>(gw, "cron.run", { id, mode: "force" }),
  runs: (gw: string, jobId: string, limit = 20) =>
    gwCall<{ entries: CronRunLogEntry[]; total: number }>(gw, "cron.runs", { jobId, limit, sortDir: "desc" }),
};

export const config = {
  get: (gw: string) =>
    gwCall<{ hash?: string; config?: Record<string, unknown>; parsed?: Record<string, unknown> } & Record<string, unknown>>(
      gw,
      "config.get",
    ),
  patch: (gw: string, raw: object, baseHash?: string, note?: string) =>
    gwCall<{ changedPaths?: string[] }>(gw, "config.patch", {
      raw: JSON.stringify(raw),
      ...(baseHash ? { baseHash } : {}),
      ...(note ? { note } : {}),
    }),
};

export const approvals = {
  resolve: (gw: string, id: string, decision: "allow-once" | "allow-always" | "deny") =>
    gwCall(gw, "exec.approval.resolve", { id, decision }),
};

/** Read-only view of a guy's workspace folder; with `sessionKey`, the folder that thread works in (ACP, Claude Code). */
export const workspace = {
  list: (gw: string, agentId: string, path = "", offset = 0, sessionKey?: string) =>
    gwCall<WorkspaceListing>(gw, "agents.workspace.list", {
      agentId,
      ...(path ? { path } : {}),
      ...(sessionKey ? { sessionKey } : {}),
      offset,
      limit: 250,
    }),
  get: (gw: string, agentId: string, path: string, sessionKey?: string) =>
    gwCall<{ agentId: string; file: WorkspaceFile }>(gw, "agents.workspace.get", { agentId, path, ...(sessionKey ? { sessionKey } : {}) }, 30_000),
};

export const skills = {
  status: (gw: string, agentId: string) =>
    gwCall<{ skills: SkillEntry[]; workspaceDir?: string }>(gw, "skills.status", { agentId }, 30_000),
  /** SKILL.md of a skill (Hermes and ACP; OpenClaw reads it from the workspace). */
  read: (gw: string, agentId: string, name: string) =>
    gwCall<{ name: string; path?: string; content: string }>(gw, "skills.read", { agentId, name }),
};

export const commands = {
  /** Slash commands the guy understands, for the composer. */
  list: (gw: string, agentId: string, sessionKey?: string) =>
    gwCall<{ commands: CommandEntry[] }>(
      gw,
      "commands.list",
      { agentId, ...(sessionKey ? { sessionKey } : {}), scope: "text", includeArgs: true },
      30_000,
    ),
};

export const tools = {
  /** What the guy can actually use in this session. */
  effective: (gw: string, agentId: string, sessionKey: string) =>
    gwCall<{ groups: ToolGroup[]; profile?: string }>(gw, "tools.effective", { agentId, sessionKey }, 30_000),
  catalog: (gw: string, agentId: string) =>
    gwCall<{ groups: ToolGroup[] }>(gw, "tools.catalog", { agentId, includePlugins: true }, 30_000),
};

export const plugins = {
  list: (gw: string) => gwCall<{ plugins: PluginEntry[] }>(gw, "plugins.list"),
};

export const mcp = {
  /** Hermes and ACP; OpenClaw keeps MCP servers in config (see abilities.ts). */
  servers: (gw: string, agentId: string) => gwCall<{ servers: McpServer[] }>(gw, "mcp.servers.list", { agentId }),
};

/** OpenClaw's browser control API, proxied through the gateway (needs admin). */
export const browser = {
  request: <T = unknown>(
    gw: string,
    method: "GET" | "POST" | "DELETE",
    path: string,
    opts: { profile?: string; body?: object; query?: Record<string, string> } = {},
  ) =>
    gwCall<T>(
      gw,
      "browser.request",
      {
        method,
        path,
        query: { ...opts.query, ...(opts.profile ? { profile: opts.profile } : {}) },
        ...(opts.body ? { body: opts.body } : {}),
      },
      30_000,
    ),
};
