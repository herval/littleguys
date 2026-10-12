//! Runtimes: the kinds of bots littleguys can drive.
//!
//! Every adapter speaks the same small, OpenClaw-shaped surface to the rest of
//! the app (Activity + webviews), so the UI doesn't care what's behind a guy:
//!
//! Calls (`Runtime::call(method, params)`):
//! - `agents.list` → `{defaultId, agents:[{id, name, identity:{name, emoji}, workspace?, model?:{primary}}]}`
//! - `sessions.list {limit?, archived?}` → `{sessions:[SessionRow-ish: {key, agentId, label?, derivedTitle?,
//!   displayName?, lastMessagePreview?, updatedAt, unread?, lastReadAt?, archived?, hasActiveRun?, status?, sessionId?}]}`:
//!   the threads that aren't archived; `archived: true` lists only archived ones, `"all"` both ([`archived_filter`])
//! - `sessions.subscribe`, `sessions.messages.subscribe {key}` → `{subscribed:true}`
//! - `sessions.create {agentId, label?}` → `{ok:true, key}`
//! - `sessions.patch {key, label?, pinned?, archived?, unread?}`, `sessions.delete {key}` (if `thread_actions`)
//! - `chat.history {sessionKey, limit?}` → `{sessionKey, messages:[ChatMessage], hasMore:false}`
//! - `chat.send {sessionKey, message, idempotencyKey, attachments?}` → `{runId, status:"started"}`
//! - `chat.abort {sessionKey, runId?}`
//! - `exec.approval.list` → `{approvals:[…]}`; `exec.approval.resolve {id, decision: allow-once|allow-always|deny}`
//! - `cron.list|add|update|remove|run|runs` (if `routines`) — same shapes as OpenClaw
//! - `models.list` (if `models`), `agents.create|update|delete`, `agents.files.get|set` (if `manage_guys`)
//! - `agents.workspace.list {agentId, path?, offset?, limit?}` → `{agentId, path, parentPath?, entries:[{path,
//!   name, kind: file|directory, size?, updatedAtMs?}], totalEntries, offset}` (dirs first, by name; root is `""`)
//! - `agents.workspace.get {agentId, path}` → `{agentId, file:{path, name, size, updatedAtMs, mimeType,
//!   encoding: utf8|base64, content}}` (if `files`; text ≤ 256 KB, images ≤ 5 MB)
//! - `skills.status {agentId}` → `{agentId, skills:[{name, skillKey, description?, source, eligible, disabled,
//!   filePath?, emoji?, missing?:{bins,env,config,os}}]}`; `skills.read {agentId, name}` → `{name, path?, content}`
//!   (if `skills`; `source:"command"` marks a slash command)
//! - `commands.list {agentId, sessionKey?}` → OpenClaw's `CommandsListResult`: `{commands:[{name, description,
//!   source: native|skill|plugin, scope:"text", acceptsArgs?, textAliases?, args?:[{name, description, type,
//!   required?, choices?:[{value,label}]}]}]}` (if `commands`; `name` has no leading `/`, `acceptsArgs` absent =
//!   unknown). `chat.send` with `/name args` runs one; a command that answers without a turn returns
//!   `status:"ok"` instead of `"started"` (its output arrives as `session.message`).
//! - `sessions.past` → `{sessions:[{sessionId, cwd, title?, updatedAt?, key, inThreads}]}`: every session the
//!   agent can resume, from every folder, newest first; `sessions.open {sessionId, cwd?, title?, updatedAt?}` →
//!   `{key}` makes one a thread that works in its own folder (if `pastSessions`)
//! - `tools.effective {agentId, sessionKey}` / `tools.catalog {agentId}` → `{agentId, groups:[{id, label, source,
//!   tools:[{id, label, description?}]}], notices?}` (if `tools`)
//! - `plugins.list` → `{plugins:[{id, name, description?, version?, enabled, state}]}`;
//!   `mcp.servers.list {agentId?}` → `{servers:[{name, transport, command?, url?, enabled?, status?, tools?}]}`
//!   (if `plugins`; never env values or secrets)
//!
//! Session keys always look like OpenClaw's: `agent:<agentId>:main` (the guy's
//! main chat), `agent:<agentId>:dashboard:<id>` (threads), `agent:<agentId>:cron:<job>:run:<id>`
//! (routine runs), `agent:<agentId>:<channel>:<id>` (messaging-channel threads).
//!
//! ChatMessage = `{role: user|assistant|toolResult, content: string | [{type:text,text} |
//! {type:thinking,thinking} | {type:toolCall,id,name,arguments} | {type:image,…}], timestamp,
//! toolCallId?, toolName?, isError?, __openclaw:{id, runId?}}`.
//!
//! Events (through `Hooks::event(name, payload, frame)`):
//! - `chat {sessionKey, runId, state: status|delta|final|aborted|error, phase?, message?, deltaText?, errorMessage?}`
//! - `session.message {sessionKey, runId?, message}` — every transcript append (user, assistant, toolResult)
//! - `agent {sessionKey, runId, stream:"tool", data:{kind:"tool", phase:start|end, toolCallId, name, title?, status?, isError?}}`
//! - `sessions.changed {sessionKey, agentId, phase: start|end|error}` (or `{reason}` for "refetch everything")
//! - `exec.approval.requested {id, agentId, sessionKey, presentation:{commandText, warningText?, allowedDecisions}}`
//! - `exec.approval.resolved {id}`
//! - `cron {action:"finished", jobId, job:{name, agentId}, status: ok|error, summary?, sessionKey?}`

pub mod acp;
pub mod claudecode;
pub mod hermes;
pub mod scheduler;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::gateway::client::{ConnStatus, GatewayClient, RpcError};
use crate::openclaw::config::Credential;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    #[default]
    Openclaw,
    Hermes,
    Acp,
    /// Every Claude Code session on this Mac (`claudecode`).
    Claudecode,
}

/// What a runtime can do; the UI hides everything else.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// Create/edit/delete guys and their files from littleguys.
    pub manage_guys: bool,
    /// Routines (cron) are available…
    pub routines: bool,
    /// …but run by littleguys itself (only while it's running).
    pub routines_local: bool,
    /// The runtime can ask before running risky commands.
    pub approvals: bool,
    /// Threads can be renamed / pinned / archived / deleted.
    pub thread_actions: bool,
    /// New threads can be started from littleguys.
    pub new_threads: bool,
    /// Images/files can be attached to messages.
    pub attachments: bool,
    /// The model can be picked per guy.
    pub models: bool,
    /// The guy's workspace can be browsed (`agents.workspace.list|get`).
    pub files: bool,
    /// Skills / slash commands can be listed (`skills.status`).
    pub skills: bool,
    /// Slash commands can be listed for the composer (`commands.list`) and run with `chat.send`.
    pub commands: bool,
    /// The guy's tools can be listed (`tools.effective` / `tools.catalog`).
    pub tools: bool,
    /// Plugins / MCP servers can be listed (`plugins.list`, `mcp.servers.list`).
    pub plugins: bool,
    /// The guy's browser can be watched and driven (OpenClaw `browser.request`).
    pub browser: bool,
    /// Past sessions from every folder can be listed and resumed (`sessions.past` / `sessions.open`).
    pub past_sessions: bool,
}

impl Capabilities {
    pub fn openclaw() -> Self {
        Self {
            manage_guys: true,
            routines: true,
            routines_local: false,
            approvals: true,
            thread_actions: true,
            new_threads: true,
            attachments: true,
            models: true,
            files: true,
            skills: true,
            commands: true,
            tools: true,
            plugins: true,
            browser: false,
            past_sessions: false,
        }
    }

    /// OpenClaw's real abilities, from the hello-ok `features.methods` list
    /// (all of them when an older gateway doesn't send one) and granted scopes.
    pub fn openclaw_from_hello(hello: &Value, scopes: &[String]) -> Self {
        let methods: Option<Vec<&str>> = hello["features"]["methods"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect());
        let has = |m: &str| methods.as_ref().is_none_or(|list| list.contains(&m));
        let admin = scopes.iter().any(|s| s == "operator.admin");
        Self {
            files: has("agents.workspace.list"),
            skills: has("skills.status"),
            commands: has("commands.list"),
            tools: has("tools.catalog") || has("tools.effective"),
            plugins: has("plugins.list"),
            browser: admin && methods.as_ref().is_some_and(|list| list.contains(&"browser.request")),
            ..Self::openclaw()
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::openclaw()
    }
}

/// One connected bot runtime. Cheap to clone.
#[derive(Clone)]
pub enum Runtime {
    OpenClaw(GatewayClient),
    Hermes(hermes::HermesClient),
    Acp(acp::AcpClient),
    ClaudeCode(claudecode::ClaudeCodeClient),
}

macro_rules! each {
    ($self:ident, $c:ident => $e:expr) => {
        match $self {
            Runtime::OpenClaw($c) => $e,
            Runtime::Hermes($c) => $e,
            Runtime::Acp($c) => $e,
            Runtime::ClaudeCode($c) => $e,
        }
    };
}

impl Runtime {
    pub fn id(&self) -> &str {
        each!(self, c => c.id())
    }

    pub fn status(&self) -> ConnStatus {
        each!(self, c => c.status())
    }

    pub fn start(&self) {
        each!(self, c => c.start())
    }

    pub fn stop(&self) {
        each!(self, c => c.stop())
    }

    pub fn reconnect_now(&self) {
        each!(self, c => c.reconnect_now())
    }

    pub fn set_name(&self, name: String) {
        each!(self, c => c.set_name(name))
    }

    pub fn set_credential_override(&self, credential: Option<Credential>) {
        each!(self, c => c.set_credential_override(credential))
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        each!(self, c => c.call(method, params).await)
    }

    pub async fn call_with_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcError> {
        each!(self, c => c.call_with_timeout(method, params, timeout).await)
    }

    pub fn as_openclaw(&self) -> Option<&GatewayClient> {
        match self {
            Runtime::OpenClaw(c) => Some(c),
            _ => None,
        }
    }
}

/// `agent:<agentId>:<rest…>` helpers shared by adapters.
pub mod keys {
    pub fn main(agent_id: &str) -> String {
        format!("agent:{agent_id}:main")
    }

    pub fn thread(agent_id: &str, id: &str) -> String {
        format!("agent:{agent_id}:dashboard:{}", escape(id))
    }

    pub fn routine_run(agent_id: &str, job_id: &str, run_id: &str) -> String {
        format!("agent:{agent_id}:cron:{}:run:{}", escape(job_id), escape(run_id))
    }

    pub fn channel(agent_id: &str, channel: &str, id: &str) -> String {
        format!("agent:{agent_id}:{}:{}", escape(channel), escape(id))
    }

    /// Ids may contain ':'; keep keys splittable.
    pub fn escape(id: &str) -> String {
        id.replace('%', "%25").replace(':', "%3A")
    }

    pub fn unescape(id: &str) -> String {
        id.replace("%3A", ":").replace("%25", "%")
    }

    /// (agentId, rest-after-agent) for `agent:<id>:<rest>`.
    pub fn split(key: &str) -> Option<(&str, &str)> {
        let rest = key.strip_prefix("agent:")?;
        rest.split_once(':')
    }

    /// The native id at the end of a thread/channel key.
    pub fn last_id(key: &str) -> Option<String> {
        key.rsplit(':').next().map(unescape)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn round_trips_ids_with_colons() {
            let k = thread("guy", "abc:123");
            assert_eq!(k, "agent:guy:dashboard:abc%3A123");
            assert_eq!(last_id(&k).as_deref(), Some("abc:123"));
            assert_eq!(split(&k), Some(("guy", "dashboard:abc%3A123")));
        }
    }
}

pub fn unsupported(method: &str) -> RpcError {
    RpcError::new("UNSUPPORTED", format!("{method} isn't supported by this bot"))
}

/// Which threads `sessions.list` wants, from its `archived` param (read the way OpenClaw
/// reads it): `Some(true)` only archived ones, `None` all of them (`"all"`), and
/// `Some(false)`, the default, only the ones that aren't.
pub fn archived_filter(params: &Value) -> Option<bool> {
    match params.get("archived") {
        Some(Value::Bool(true)) => Some(true),
        Some(Value::String(s)) if s == "all" => None,
        _ => Some(false),
    }
}

/// What you set on a thread from littleguys (`sessions.patch`), kept by littleguys for
/// runtimes that don't keep it themselves (ACP, Claude Code).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadFlags {
    /// Your name for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub unread: bool,
    /// When it was last marked read (ms): what came after is new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read_at: Option<u64>,
    #[serde(default)]
    pub pinned: bool,
    /// Your Archive (`true`) or Unarchive (`false`). `None`: you haven't said, so the
    /// runtime decides (ACP: not archived; Claude Code: archived once it's done).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
}

impl ThreadFlags {
    /// Applies `sessions.patch {label?, unread?, pinned?, archived?}`; returns whether anything changed.
    pub fn patch(&mut self, params: &Value) -> bool {
        let before = self.clone();
        if let Some(label) = params.get("label") {
            self.label = label.as_str().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string);
        }
        if let Some(unread) = params.get("unread").and_then(Value::as_bool) {
            self.unread = unread;
            if !unread {
                self.last_read_at = Some(crate::gateway::identity::now_ms());
            }
        }
        if let Some(pinned) = params.get("pinned").and_then(Value::as_bool) {
            self.pinned = pinned;
        }
        if let Some(archived) = params.get("archived").and_then(Value::as_bool) {
            self.archived = Some(archived);
        }
        *self != before
    }

    /// Adds the session-row fields they show as: `label`, `unread`, `lastReadAt` and `pinned`
    /// (`archived` is the runtime's to work out).
    pub fn add_to(&self, row: &mut Value) {
        row["label"] = serde_json::json!(self.label);
        row["unread"] = Value::Bool(self.unread);
        row["lastReadAt"] = serde_json::json!(self.last_read_at);
        row["pinned"] = Value::Bool(self.pinned);
    }
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/var/empty"))
}

/// Runs file-reading work off the async runtime.
pub async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, RpcError> + Send + 'static) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(work).await.map_err(|e| RpcError::new("INTERNAL", e.to_string()))?
}

pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

pub fn parse_iso_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis().max(0) as u64)
}

/// `text`, trimmed, cut to at most `max` characters (an ellipsis included).
pub fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Claude's face, the Claude Code mascot, as a data URL for `identity.avatarUrl`.
pub fn claude_face() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        let svg = include_str!("../../assets/claude-code.svg");
        format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn thread_flags_patch_and_show() {
        let mut flags = ThreadFlags::default();
        assert!(!flags.patch(&json!({ "key": "k" })));
        assert!(flags.patch(&json!({ "label": "  Taxes ", "pinned": true, "unread": true })));
        assert_eq!(flags.label.as_deref(), Some("Taxes"));
        assert!(flags.patch(&json!({ "unread": false, "archived": true })));
        assert!(flags.last_read_at.is_some());
        assert_eq!(flags.archived, Some(true));
        assert!(flags.patch(&json!({ "label": null })));
        assert_eq!(flags.label, None);

        let mut row = json!({ "key": "k", "archived": true });
        flags.add_to(&mut row);
        assert_eq!(row["pinned"], true);
        assert_eq!(row["unread"], false);
        assert_eq!(row["label"], Value::Null);
        assert_eq!(row["archived"], true);
    }

    #[test]
    fn truncate_keeps_to_max_characters() {
        assert_eq!(truncate("  short  ", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("ab cdefg", 4), "ab…");
    }

    #[test]
    fn archived_filter_reads_openclaws_param() {
        assert_eq!(archived_filter(&json!({})), Some(false));
        assert_eq!(archived_filter(&json!({ "archived": false })), Some(false));
        assert_eq!(archived_filter(&json!({ "archived": true })), Some(true));
        assert_eq!(archived_filter(&json!({ "archived": "all" })), None);
    }
}
