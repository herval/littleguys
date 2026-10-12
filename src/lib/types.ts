// Gateway protocol types come straight from OpenClaw's published package so
// the UI tracks the real wire format. littleguys-only shapes live below.
export type {
  AgentEvent,
  AgentSummary,
  ChatEvent,
  CronJob,
  CronRunLogEntry,
  ModelChoice,
  SessionRow,
} from "@openclaw/gateway-protocol";

import type { AgentSummary, SessionRow } from "@openclaw/gateway-protocol";

export type Phase =
  | "starting"
  | "connecting"
  | "connected"
  | "pairing"
  | "auth-failed"
  | "offline"
  | "no-config"
  | "error";

export type RuntimeKind = "openclaw" | "hermes" | "acp" | "claudecode";

/** What a bot runtime can do; the UI hides the rest (see src-tauri/src/runtime/mod.rs). */
export interface Capabilities {
  manageGuys: boolean;
  routines: boolean;
  routinesLocal: boolean;
  approvals: boolean;
  threadActions: boolean;
  newThreads: boolean;
  attachments: boolean;
  models: boolean;
  files: boolean;
  skills: boolean;
  /** Slash commands can be listed for the composer (`commands.list`). */
  commands: boolean;
  tools: boolean;
  plugins: boolean;
  browser: boolean;
  /** Past sessions from every folder can be listed and resumed (`sessions.past` / `sessions.open`, ACP). */
  pastSessions: boolean;
}

/** A session the agent can resume, from any folder (`sessions.past`). */
export interface PastSession {
  sessionId: string;
  cwd: string;
  title?: string | null;
  updatedAt?: number | null;
  /** The thread it is (or will be once opened). */
  key: string;
  /** Already in the thread list (opened before, or in the guy's own folder). */
  inThreads: boolean;
}

export interface ConnStatus {
  gatewayId: string;
  gatewayName: string;
  kind: RuntimeKind;
  capabilities: Capabilities;
  phase: Phase;
  mode: "local" | "remote" | null;
  url: string | null;
  message: string | null;
  detailCode: string | null;
  requestId: string | null;
  deviceId: string;
  serverVersion: string | null;
  configVersion: string | null;
  configPath: string;
  scopes: string[];
  retryAtMs: number | null;
  credentialIssue: string | null;
  hello: HelloSnapshot | null;
}

export interface HelloSnapshot {
  policy?: {
    maxPayload?: number;
    attachments?: { maxBytes?: number; maxImageBytes?: number };
  };
  snapshot?: { sessionDefaults?: { defaultAgentId?: string; mainSessionKey?: string } };
  features?: { methods?: string[] };
  /** Claude Code: littleguys answers background sessions' permission prompts. */
  approvals?: boolean;
}

export interface ExecApproval {
  id?: string;
  approvalId?: string;
  agentId?: string;
  sessionKey?: string;
  command?: string;
  request?: { id?: string; command?: string; agentId?: string; sessionKey?: string; cwd?: string };
  presentation?: { commandText?: string; warningText?: string | null; allowedDecisions?: string[] };
  [key: string]: unknown;
}

/** One gateway's slice of the roster (see src-tauri/src/activity.rs). */
export interface GatewayRoster {
  id: string;
  name: string;
  enabled: boolean;
  status: ConnStatus;
  loaded: boolean;
  defaultAgentId: string | null;
  agents: AgentSummary[];
  sessions: SessionRow[];
  running: string[];
  approvals: ExecApproval[];
}

export interface Roster {
  gateways: GatewayRoster[];
  /** Guys hidden in Settings, as `gatewayId/agentId`. */
  hidden: string[];
  /** Guys with a photo picked in littleguys (`gatewayId/agentId`); it's already their `identity.avatarUrl`. */
  faces: string[];
  updatedAt: number;
}

/** A little guy = an OpenClaw agent on a particular gateway. */
export type Guy = AgentSummary & {
  gatewayId: string;
  gatewayName: string;
  /** `${gatewayId}/${agentId}` — unique across gateways. */
  uid: string;
};

export interface AcpConfig {
  transport: "stdio" | "websocket";
  command?: string | null;
  args: string[];
  env: Record<string, string>;
  url?: string | null;
  cwd?: string | null;
  agentName: string;
  emoji?: string | null;
  registryId?: string | null;
}

export interface HermesConfig {
  mode: "local" | "api";
  url?: string | null;
  profile?: string | null;
}

/** Claude Code on this Mac (every session is a thread). */
export interface ClaudeCodeConfig {
  /** littleguys answers background sessions' permission prompts. */
  approvals: boolean;
}

/** What `claude_code_detect` found. */
export interface ClaudeCodeProbe {
  installed: boolean;
  version?: string | null;
  plugin?: { enabled: boolean; folder?: string | null } | null;
}

export interface GatewayEntry {
  id: string;
  name: string;
  url: string | null;
  enabled: boolean;
  addedAtMs: number;
  kind: RuntimeKind;
  acp?: AcpConfig;
  hermes?: HermesConfig;
  claude?: ClaudeCodeConfig;
}

export interface RegistryAgent {
  id: string;
  name: string;
  description: string;
  version: string;
  website?: string | null;
  icon?: string | null;
  command: string;
  args: string[];
  env: Record<string, string>;
}

export interface HermesProbe {
  url: string;
  version?: string | null;
  mode: "local" | "api";
  authRequired: boolean;
}

export interface DiscoveredGateway {
  name: string;
  url: string;
  via: "tailscale" | "bonjour";
  host: string;
  addedAs: string | null;
}

export interface Navigate {
  gatewayId?: string | null;
  agentId?: string | null;
  sessionKey?: string | null;
  view?: string | null;
}

/** Self-update from GitHub releases (src-tauri/src/updater.rs); pushed as `lg:update`. */
export interface UpdateState {
  current: string;
  phase: "idle" | "checking" | "downloading" | "ready" | "error" | "disabled";
  version: string | null;
  error: string | null;
  checkedAt: number | null;
}

/** A transcript message as returned by chat.history / chat events. */
export interface ChatMessage {
  role: "user" | "assistant" | "toolResult" | "system" | string;
  content: string | ContentBlock[];
  timestamp?: number;
  toolCallId?: string;
  toolName?: string;
  isError?: boolean;
  stopReason?: string;
  model?: string;
  /** Who sent a user message that came in from a channel (Slack, Teams…). */
  senderLabel?: string;
  /** On a tool result: the files that tool changed. */
  diffs?: ToolDiff[];
  /** On a question tool's result: each question → the answer given. */
  answers?: Record<string, string>;
  __openclaw?: { id?: string; seq?: number; runId?: string; kind?: string };
}

/**
 * A file a tool changed: unified-diff hunks (Claude Code's own patch) or the
 * text before and after (ACP `diff` content; `oldText: null` for a new file).
 */
export interface ToolDiff {
  path: string;
  hunks?: Array<{ oldStart: number; newStart: number; lines: string[] }>;
  oldText?: string | null;
  newText?: string;
}

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "thinking"; thinking: string }
  | { type: "toolCall"; id: string; name: string; arguments?: unknown }
  | {
      type: "image";
      url?: string;
      openUrl?: string;
      artifactId?: string;
      alt?: string;
      mimeType?: string;
      data?: string;
      source?: { type: string; media_type?: string; data?: string };
    }
  | { type: "attachment"; attachment: { artifactId?: string; url?: string; label?: string; mimeType?: string } }
  | { type: string; [key: string]: unknown };

export interface Attachment {
  type: "image" | "file";
  mimeType: string;
  fileName: string;
  content: string; // base64
  sizeBytes: number;
  previewUrl?: string;
}

export interface DoctorIssue {
  id: string;
  title: string;
  detail: string;
  fix: "install" | "update" | "start" | "onboard" | "pairing" | "credential" | null;
}

export interface DoctorReport {
  mode: "local" | "remote" | null;
  configPath: string;
  configExists: boolean;
  configVersion: string | null;
  cliPath: string | null;
  cliVersion: string | null;
  cliProblem: string | null;
  gatewayUrl: string | null;
  gatewayHealthy: boolean | null;
  serviceInstalled: boolean;
  service: { loaded: boolean; running: boolean; pid: number | null; lastExitCode: number | null };
  connected: boolean;
  issues: DoctorIssue[];
  logTail: string | null;
}

// ---- per-guy workspace, skills & tools, browser (see src-tauri/src/runtime/mod.rs) ----

export interface WorkspaceEntry {
  /** Workspace-relative, `/`-separated. */
  path: string;
  name: string;
  kind: "file" | "directory";
  size?: number;
  updatedAtMs?: number;
}

export interface WorkspaceListing {
  agentId: string;
  /** "" is the workspace root. */
  path: string;
  parentPath?: string;
  entries: WorkspaceEntry[];
  totalEntries: number;
  offset: number;
  /** The folder listed, when threads each work in their own (ACP, Claude Code). */
  workspaceDir?: string;
}

export interface WorkspaceFile {
  path: string;
  name: string;
  size: number;
  updatedAtMs?: number;
  mimeType: string;
  encoding: "utf8" | "base64";
  content: string;
  truncated?: boolean;
}

export interface SkillEntry {
  name: string;
  skillKey?: string;
  description?: string;
  /** Where it comes from (bundled, workspace, personal…); `command` marks a slash command. */
  source?: string;
  category?: string;
  emoji?: string;
  eligible?: boolean;
  disabled?: boolean;
  blockedByAllowlist?: boolean;
  blockedByAgentFilter?: boolean;
  always?: boolean;
  filePath?: string;
  homepage?: string;
  hint?: string;
  /** Hermes: how many times it was used. */
  usage?: number | string;
  primaryEnv?: string;
  missing?: Record<string, string[] | undefined>;
  install?: Array<{ id: string; kind: string; label: string; bins?: string[] }>;
}

/** A slash command (OpenClaw's `CommandEntry`; every runtime answers `commands.list` in this shape). */
export interface CommandEntry {
  /** Without the leading `/`. */
  name: string;
  description?: string;
  source?: "native" | "skill" | "plugin";
  category?: string;
  /** Other ways to type it, with the `/` (OpenClaw). */
  textAliases?: string[];
  /** Unknown when absent. */
  acceptsArgs?: boolean;
  args?: Array<{
    name: string;
    description?: string;
    type?: string;
    required?: boolean;
    choices?: Array<{ value: string; label: string }>;
    dynamic?: boolean;
  }>;
}

export interface ToolEntry {
  id: string;
  label?: string;
  description?: string;
  source?: string;
  pluginId?: string;
  mcpServer?: string;
  risk?: string;
  deniedBySession?: boolean;
}

export interface ToolGroup {
  id: string;
  label?: string;
  description?: string;
  source?: string;
  pluginId?: string;
  enabled?: boolean;
  tools: ToolEntry[];
}

export interface PluginEntry {
  id: string;
  name: string;
  description?: string;
  version?: string;
  enabled?: boolean;
  installed?: boolean;
  state?: string;
  error?: string;
  kind?: string[];
}

export interface McpServer {
  name: string;
  transport?: string;
  command?: string;
  url?: string;
  enabled?: boolean;
  status?: string;
  tools?: number | string[];
  source?: string;
}

export interface BrowserTab {
  targetId: string;
  title?: string;
  url?: string;
  type?: string;
}
