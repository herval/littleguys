//! Routines for bots that have no scheduler of their own (most ACP agents):
//! littleguys keeps the schedule and, when a routine is due, sends its prompt
//! to the bot. Jobs and runs use OpenClaw's `cron.*` shapes so the Routines UI
//! works unchanged. Runs only happen while littleguys is running; missed runs
//! are skipped, not replayed.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::gateway::client::RpcError;
use crate::gateway::identity::{now_ms, write_private};

/// How a routine's turn is executed by the owning runtime.
pub trait RoutineRunner: Send + Sync {
    /// Send `message` to `agent_id`, in a fresh thread (`target == "isolated"`)
    /// or in `session:<key>`. Resolves when the bot's turn ends.
    fn run_turn(&self, agent_id: String, target: String, job_id: String, message: String) -> BoxFuture<'static, Result<TurnOutcome, String>>;
    /// Emit an OpenClaw-shaped event (the scheduler emits `cron`).
    fn emit(&self, name: &str, payload: Value);
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub session_key: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub name: String,
    pub agent_id: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// `{kind:"cron", expr, tz?}` | `{kind:"every", everyMs, anchorMs?}` | `{kind:"at", at}`
    pub schedule: Value,
    /// `{kind:"agentTurn", message}`
    pub payload: Value,
    #[serde(default = "isolated")]
    pub session_target: String,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub next_run_at_ms: Option<u64>,
    #[serde(default)]
    pub last_run_at_ms: Option<u64>,
    #[serde(default)]
    pub last_run_status: Option<String>,
    #[serde(default)]
    pub last_run_error: Option<String>,
}

fn yes() -> bool {
    true
}

fn isolated() -> String {
    "isolated".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEntry {
    pub ts: u64,
    pub job_id: String,
    pub job_name: String,
    pub run_id: String,
    pub status: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub session_key: Option<String>,
    pub run_at_ms: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Store {
    jobs: Vec<Job>,
    #[serde(default)]
    runs: HashMap<String, Vec<RunEntry>>,
}

const MAX_RUNS_PER_JOB: usize = 50;

pub struct LocalScheduler {
    path: PathBuf,
    store: Mutex<Store>,
    running: Mutex<HashSet<String>>,
    stopped: AtomicBool,
    dirty: AtomicBool,
    runner: Mutex<Option<Arc<dyn RoutineRunner>>>,
    wake: tokio::sync::Notify,
}

impl LocalScheduler {
    /// Routines for one bot, persisted at `<data_dir>/routines-<gateway_id>.json`.
    pub fn new(data_dir: &std::path::Path, gateway_id: &str) -> Arc<Self> {
        let path = data_dir.join(format!("routines-{gateway_id}.json"));
        let mut store: Store = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        // Missed runs are skipped: schedule everything from now.
        let now = now_ms();
        for job in store.jobs.iter_mut() {
            job.next_run_at_ms = if job.enabled { next_run(&job.schedule, now, job.last_run_at_ms) } else { None };
        }
        Arc::new(Self {
            path,
            store: Mutex::new(store),
            running: Mutex::new(HashSet::new()),
            stopped: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            runner: Mutex::new(None),
            wake: tokio::sync::Notify::new(),
        })
    }

    /// Start firing due routines through `runner`.
    pub fn start(self: &Arc<Self>, runner: Arc<dyn RoutineRunner>) {
        *self.runner.lock().unwrap() = Some(runner);
        self.stopped.store(false, Ordering::SeqCst);
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            while !this.stopped.load(Ordering::SeqCst) {
                if this.dirty.load(Ordering::SeqCst) {
                    let store = this.store.lock().unwrap();
                    if let Err(e) = this.persist(&store) { log::warn!("{e}"); }
                }
                let now = now_ms();
                let due: Vec<String> = {
                    let store = this.store.lock().unwrap();
                    let running = this.running.lock().unwrap();
                    store
                        .jobs
                        .iter()
                        .filter(|j| j.enabled && j.next_run_at_ms.is_some_and(|t| t <= now) && !running.contains(&j.id))
                        .map(|j| j.id.clone())
                        .collect()
                };
                for id in due {
                    this.clone().spawn_run(id);
                }
                let next = this.store.lock().unwrap().jobs.iter().filter_map(|j| j.next_run_at_ms).min();
                let wait = next.map_or(30_000, |t| t.saturating_sub(now_ms()).clamp(500, 30_000));
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
                    _ = this.wake.notified() => {}
                }
            }
        });
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    fn persist(&self, store: &Store) -> Result<(), RpcError> {
        let result = serde_json::to_vec_pretty(store).map_err(anyhow::Error::from)
            .and_then(|bytes| write_private(&self.path, &bytes));
        self.dirty.store(result.is_err(), Ordering::SeqCst);
        result.map_err(|e| RpcError::new("PERSISTENCE", format!("Couldn't save routines: {e}")))
    }

    fn spawn_run(self: Arc<Self>, job_id: String) {
        let Some(runner) = self.runner.lock().unwrap().clone() else { return };
        let job = {
            let mut store = self.store.lock().unwrap();
            let Some(job) = store.jobs.iter_mut().find(|j| j.id == job_id) else { return };
            // Advance the schedule before running so a slow run can't double-fire.
            let now = now_ms();
            job.next_run_at_ms = next_run(&job.schedule, now + 1_000, Some(now));
            job.clone()
        };
        self.running.lock().unwrap().insert(job.id.clone());
        let message = job.payload.get("message").and_then(Value::as_str).unwrap_or_default().to_string();
        runner.emit("cron", json!({ "action": "started", "jobId": job.id, "job": public_job(&job) }));
        tauri::async_runtime::spawn(async move {
            let started = now_ms();
            let run_id = format!("local:{}:{started}", job.id);
            let result = runner.run_turn(job.agent_id.clone(), job.session_target.clone(), job.id.clone(), message).await;
            let (mut status, summary, mut error, session_key) = match result {
                Ok(o) => ("ok", o.summary, None, Some(o.session_key)),
                Err(e) => ("error", None, Some(e), None),
            };
            let entry = RunEntry {
                ts: now_ms(),
                job_id: job.id.clone(),
                job_name: job.name.clone(),
                run_id,
                status: status.into(),
                summary: summary.clone(),
                error: error.clone(),
                session_key: session_key.clone(),
                run_at_ms: started,
                duration_ms: now_ms().saturating_sub(started),
            };
            let updated = {
                let mut store = self.store.lock().unwrap();
                let runs = store.runs.entry(job.id.clone()).or_default();
                runs.insert(0, entry);
                runs.truncate(MAX_RUNS_PER_JOB);
                let updated = store.jobs.iter_mut().find(|j| j.id == job.id).map(|j| {
                    j.last_run_at_ms = Some(started);
                    j.last_run_status = Some(status.into());
                    j.last_run_error = error.clone();
                    // One-shot routines are done after running.
                    if j.schedule.get("kind").and_then(Value::as_str) == Some("at") {
                        j.enabled = false;
                        j.next_run_at_ms = None;
                    }
                    j.clone()
                });
                if let Err(e) = self.persist(&store) {
                    status = "error";
                    error = Some(format!("The run finished, but its history couldn't be saved: {e}"));
                }
                updated
            };
            self.running.lock().unwrap().remove(&job.id);
            runner.emit(
                "cron",
                json!({
                    "action": "finished",
                    "jobId": job.id,
                    "job": public_job(updated.as_ref().unwrap_or(&job)),
                    "status": status,
                    "summary": error.or(summary),
                    "sessionKey": session_key,
                }),
            );
        });
    }

    /// Answer a `cron.*` call.
    pub async fn handle(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = params.get("id").or_else(|| params.get("jobId")).and_then(Value::as_str).map(str::to_string);
        match method {
            "cron.list" => {
                let agent = params.get("agentId").and_then(Value::as_str);
                let store = self.store.lock().unwrap();
                let jobs: Vec<Value> = store
                    .jobs
                    .iter()
                    .filter(|j| agent.is_none_or(|a| j.agent_id == a))
                    .map(public_job)
                    .collect();
                Ok(json!({ "jobs": jobs, "total": jobs.len(), "hasMore": false }))
            }
            "cron.get" => {
                let store = self.store.lock().unwrap();
                store.jobs.iter().find(|j| Some(&j.id) == id.as_ref()).map(public_job).ok_or_else(not_found)
            }
            "cron.add" => {
                let now = now_ms();
                let schedule = params.get("schedule").cloned().ok_or_else(|| bad("schedule is required"))?;
                validate_schedule(&schedule)?;
                let job = Job {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: params.get("name").and_then(Value::as_str).unwrap_or("Routine").to_string(),
                    agent_id: params.get("agentId").and_then(Value::as_str).ok_or_else(|| bad("agentId is required"))?.to_string(),
                    enabled: params.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                    next_run_at_ms: next_run(&schedule, now, None),
                    schedule,
                    payload: params.get("payload").cloned().unwrap_or_else(|| json!({ "kind": "agentTurn", "message": "" })),
                    session_target: params.get("sessionTarget").and_then(Value::as_str).unwrap_or("isolated").to_string(),
                    created_at_ms: now,
                    updated_at_ms: now,
                    last_run_at_ms: None,
                    last_run_status: None,
                    last_run_error: None,
                };
                let out = public_job(&job);
                let mut saved = self.store.lock().unwrap();
                let mut store = saved.clone();
                store.jobs.push(job);
                self.persist(&store)?;
                *saved = store;
                drop(saved);
                self.wake.notify_one();
                self.announce("added", &out);
                Ok(out)
            }
            "cron.update" => {
                let id = id.ok_or_else(|| bad("id is required"))?;
                let patch = params.get("patch").cloned().unwrap_or(Value::Null);
                if let Some(s) = patch.get("schedule") {
                    validate_schedule(s)?;
                }
                let out = {
                    let mut saved = self.store.lock().unwrap();
                    let mut store = saved.clone();
                    let job = store.jobs.iter_mut().find(|j| j.id == id).ok_or_else(not_found)?;
                    if let Some(v) = patch.get("name").and_then(Value::as_str) {
                        job.name = v.to_string();
                    }
                    if let Some(v) = patch.get("enabled").and_then(Value::as_bool) {
                        job.enabled = v;
                    }
                    if let Some(v) = patch.get("schedule") {
                        job.schedule = v.clone();
                    }
                    if let Some(v) = patch.get("payload") {
                        job.payload = v.clone();
                    }
                    if let Some(v) = patch.get("sessionTarget").and_then(Value::as_str) {
                        job.session_target = v.to_string();
                    }
                    job.updated_at_ms = now_ms();
                    job.next_run_at_ms = if job.enabled { next_run(&job.schedule, now_ms(), job.last_run_at_ms) } else { None };
                    let out = public_job(job);
                    self.persist(&store)?;
                    *saved = store;
                    out
                };
                self.wake.notify_one();
                self.announce("updated", &out);
                Ok(out)
            }
            "cron.remove" => {
                let id = id.ok_or_else(|| bad("id is required"))?;
                let mut saved = self.store.lock().unwrap();
                let mut store = saved.clone();
                let before = store.jobs.len();
                store.jobs.retain(|j| j.id != id);
                store.runs.remove(&id);
                if store.jobs.len() == before {
                    return Err(not_found());
                }
                self.persist(&store)?;
                *saved = store;
                drop(saved);
                self.announce("removed", &json!({ "id": id }));
                Ok(json!({ "ok": true, "removed": true }))
            }
            "cron.run" => {
                let id = id.ok_or_else(|| bad("id is required"))?;
                if self.running.lock().unwrap().contains(&id) {
                    return Err(RpcError::new("CONFLICT", "That routine is already running."));
                }
                if !self.store.lock().unwrap().jobs.iter().any(|j| j.id == id) {
                    return Err(not_found());
                }
                let run_id = format!("local:{id}:{}", now_ms());
                self.clone().spawn_run(id);
                Ok(json!({ "ok": true, "enqueued": true, "runId": run_id }))
            }
            "cron.runs" => {
                let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
                let store = self.store.lock().unwrap();
                let entries: Vec<RunEntry> = match &id {
                    Some(id) => store.runs.get(id).cloned().unwrap_or_default(),
                    None => {
                        let mut all: Vec<RunEntry> = store.runs.values().flatten().cloned().collect();
                        all.sort_by(|a, b| b.ts.cmp(&a.ts));
                        all
                    }
                };
                let total = entries.len();
                Ok(json!({ "entries": entries.into_iter().take(limit).collect::<Vec<_>>(), "total": total }))
            }
            "cron.status" => Ok(json!({ "enabled": true, "local": true })),
            other => Err(crate::runtime::unsupported(other)),
        }
    }

    fn announce(&self, action: &str, job: &Value) {
        if let Some(runner) = self.runner.lock().unwrap().clone() {
            runner.emit("cron", json!({ "action": action, "job": job }));
        }
    }
}

fn public_job(job: &Job) -> Value {
    let mut v = serde_json::to_value(job).unwrap_or(Value::Null);
    v["state"] = json!({ "nextRunAtMs": job.next_run_at_ms, "lastRunAtMs": job.last_run_at_ms });
    v["runsLocally"] = json!(true);
    v
}

fn bad(msg: &str) -> RpcError {
    RpcError::new("INVALID_REQUEST", msg)
}

fn not_found() -> RpcError {
    RpcError::new("NOT_FOUND", "No such routine.")
}

fn validate_schedule(schedule: &Value) -> Result<(), RpcError> {
    match schedule.get("kind").and_then(Value::as_str) {
        Some("cron") => {
            let expr = schedule.get("expr").and_then(Value::as_str).unwrap_or_default();
            croner::Cron::from_str(expr).map_err(|e| bad(&format!("invalid cron expression: {e}")))?;
            if let Some(tz) = schedule.get("tz").and_then(Value::as_str) {
                chrono_tz::Tz::from_str(tz).map_err(|_| bad(&format!("unknown time zone {tz}")))?;
            }
            Ok(())
        }
        Some("every") => match schedule.get("everyMs").and_then(Value::as_u64) {
            Some(ms) if ms >= 60_000 => Ok(()),
            _ => Err(bad("routines can run at most once a minute")),
        },
        Some("at") => schedule
            .get("at")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|_| ())
            .ok_or_else(|| bad("invalid date for a one-time routine")),
        _ => Err(bad("littleguys can run cron, every-N and one-time routines")),
    }
}

fn local_tz() -> chrono_tz::Tz {
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| p.to_string_lossy().split("zoneinfo/").nth(1).map(str::to_string))
        .and_then(|name| chrono_tz::Tz::from_str(&name).ok())
        .unwrap_or(chrono_tz::UTC)
}

/// Next time (epoch ms, strictly after `after_ms`) the schedule fires.
pub fn next_run(schedule: &Value, after_ms: u64, last_run_ms: Option<u64>) -> Option<u64> {
    use chrono::{TimeZone, Utc};
    match schedule.get("kind").and_then(Value::as_str)? {
        "cron" => {
            let cron = croner::Cron::from_str(schedule.get("expr")?.as_str()?).ok()?;
            let tz = schedule
                .get("tz")
                .and_then(Value::as_str)
                .and_then(|t| chrono_tz::Tz::from_str(t).ok())
                .unwrap_or_else(local_tz);
            let after = Utc.timestamp_millis_opt(after_ms as i64).single()?.with_timezone(&tz);
            let next = cron.find_next_occurrence(&after, false).ok()?;
            Some(next.timestamp_millis() as u64)
        }
        "every" => {
            let every = schedule.get("everyMs")?.as_u64()?.max(60_000);
            let anchor = schedule.get("anchorMs").and_then(Value::as_u64).or(last_run_ms).unwrap_or(after_ms);
            if anchor > after_ms {
                return Some(anchor);
            }
            let periods = (after_ms - anchor) / every + 1;
            Some(anchor + periods * every)
        }
        "at" => {
            let at = chrono::DateTime::parse_from_rfc3339(schedule.get("at")?.as_str()?).ok()?;
            let ms = at.timestamp_millis() as u64;
            (ms > after_ms && last_run_ms.is_none_or(|l| l < ms)).then_some(ms)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_add_does_not_commit_a_routine() {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        std::fs::write(&dir, "not a directory").unwrap();
        let scheduler = LocalScheduler::new(&dir, "test");
        let result = scheduler.handle("cron.add", json!({
            "agentId": "main", "schedule": {"kind": "every", "everyMs": 60000}
        })).await;
        assert_eq!(result.unwrap_err().code, "PERSISTENCE");
        assert!(scheduler.store.lock().unwrap().jobs.is_empty());
        std::fs::remove_file(dir).unwrap();
    }

    #[test]
    fn cron_next_run_respects_timezone() {
        // 2026-10-02T12:00:00Z; daily 9:00 in São Paulo (UTC-3) = 12:00Z → next is tomorrow 12:00Z.
        let after = 1_791_000_000_000u64 - (1_791_000_000_000u64 % 86_400_000) + 12 * 3_600_000;
        let s = json!({ "kind": "cron", "expr": "0 9 * * *", "tz": "America/Sao_Paulo" });
        let next = next_run(&s, after, None).unwrap();
        assert_eq!(next - after, 86_400_000);
    }

    #[test]
    fn every_counts_from_the_last_run() {
        let s = json!({ "kind": "every", "everyMs": 3_600_000 });
        assert_eq!(next_run(&s, 10_000_000, Some(9_000_000)), Some(12_600_000));
    }

    #[test]
    fn one_time_routines_fire_once() {
        let s = json!({ "kind": "at", "at": "2030-01-01T09:00:00Z" });
        let at = 1_893_488_400_000u64;
        assert_eq!(next_run(&s, at - 1, None), Some(at));
        assert_eq!(next_run(&s, at + 1, None), None);
        assert_eq!(next_run(&s, at - 1, Some(at)), None);
    }

    #[test]
    fn rejects_bad_schedules() {
        assert!(validate_schedule(&json!({ "kind": "cron", "expr": "nope" })).is_err());
        assert!(validate_schedule(&json!({ "kind": "every", "everyMs": 1000 })).is_err());
        assert!(validate_schedule(&json!({ "kind": "cron", "expr": "0 9 * * 1-5", "tz": "Europe/Lisbon" })).is_ok());
    }
}
