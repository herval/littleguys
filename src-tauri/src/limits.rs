//! Usage limits: how much of a provider's rate limit is used and when it resets,
//! for the thin line on a guy's row in the popover. Read when the popover asks for
//! them (when it opens, then once a minute while it's open); nothing here polls.
//!
//! - **Codex** logs them: `token_count` events in
//!   `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`. Only the ends of the newest few
//!   logs are read.
//! - **Claude Code** only tells status lines: hook input doesn't have them, transcripts
//!   only mention a limit once it's been hit, and a plugin can't provide a status line.
//!   So "Show usage limits" in Settings (an opt-in) chains the person's own status line:
//!   `statusLine` in Claude Code's settings runs a small wrapper in littleguys' data dir,
//!   which hands the JSON to the app over the hook channel, then runs the original.
//!   [`set_status_line`] puts the original back, byte for byte.
//!
//! Nothing here reads the Keychain or credential files or calls a usage endpoint.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::gateway::client::RpcError;
use crate::gateway::identity::now_ms;
use crate::runtime::{blocking, claudecode, home, parse_iso_ms};

/// Where the wrapper reports on the hook channel. (Its scripts spell this out.)
pub const CLAUDE_REPORT_PATH: &str = "/limits/claude";

/// Day folders of Codex logs to look through, newest first.
const CODEX_DAYS: usize = 7;
/// Logs to try: plenty hold no reading (subagents', sessions that never got a reply).
const CODEX_LOGS: usize = 16;
/// How much of a log's end is read.
const CODEX_TAIL_BYTES: u64 = 256 << 10;

const STATUS_LINE_DIR: &str = "claude-statusline";
const STATUS_LINE_KEY: &str = "statusLine";
const BACKUP_FILE: &str = "backup.json";

#[cfg(unix)]
const WRAPPER: (&str, &str) = ("statusline.sh", include_str!("../claude-plugin/statusline/statusline.sh"));
#[cfg(windows)]
const WRAPPER: (&str, &str) = ("statusline.ps1", include_str!("../claude-plugin/statusline/statusline.ps1"));
/// The original status line, as the wrapper runs it.
#[cfg(unix)]
const OWN_FILE: &str = "own.sh";
#[cfg(windows)]
const OWN_FILE: &str = "own.ps1";

/// One limit window, as last reported.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Limit {
    pub used_percent: f64,
    pub resets_at_ms: u64,
    /// When this was reported, so an old reading isn't passed off as current.
    pub read_at_ms: u64,
}

/// `used_key` and `resets_at` (Unix seconds) of one window; `None` unless both are there.
fn window(value: &Value, used_key: &str, read_at_ms: u64) -> Option<Limit> {
    let used = value.get(used_key)?.as_f64().filter(|used| used.is_finite())?;
    let resets_at = value.get("resets_at")?.as_f64().filter(|at| *at > 0.0)?;
    Some(Limit { used_percent: used.clamp(0.0, 100.0), resets_at_ms: (resets_at * 1000.0) as u64, read_at_ms })
}

/// The window closest to running out, leaving out those already over.
fn tightest(windows: impl IntoIterator<Item = Limit>, now_ms: u64) -> Option<Limit> {
    windows.into_iter().filter(|w| w.resets_at_ms > now_ms).max_by(|a, b| {
        a.used_percent.total_cmp(&b.used_percent).then(a.resets_at_ms.cmp(&b.resets_at_ms))
    })
}

/// The limit to show for `provider` ("claude", "codex"), if there's a current one.
pub fn read(provider: &str, now_ms: u64) -> Option<Limit> {
    match provider {
        "claude" => {
            let latest = *CLAUDE.lock().unwrap();
            tightest(latest.five_hour.into_iter().chain(latest.seven_day), now_ms)
        }
        "codex" => codex_limit(&codex_sessions(), now_ms),
        _ => None,
    }
}

/// How much of `provider`'s rate limit is used; null when there's no current reading.
#[tauri::command]
pub async fn limits_read(provider: String) -> Result<Option<Limit>, RpcError> {
    blocking(move || Ok(read(&provider, now_ms()))).await
}

// ---- Codex: session logs ------------------------------------------------------------

fn codex_sessions() -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(".codex"),
    }
    .join("sessions")
}

/// The windows in one log line, if it's a `token_count` event that has any.
fn codex_reading(line: &str) -> Option<Vec<Limit>> {
    if !line.contains("\"rate_limits\"") {
        return None;
    }
    let event: Value = serde_json::from_str(line).ok()?;
    let payload = event.get("payload")?;
    if event.get("type")? != "event_msg" || payload.get("type")? != "token_count" {
        return None;
    }
    let limits = payload.get("rate_limits")?;
    // "codex" is the main limit, and the only id seen in real logs. Other ids are extra metered ones.
    if limits.get("limit_id").and_then(Value::as_str).is_some_and(|id| id != "codex") {
        return None;
    }
    let read_at_ms = event.get("timestamp")?.as_str().and_then(parse_iso_ms)?;
    let windows: Vec<Limit> =
        ["primary", "secondary"].iter().filter_map(|key| window(limits.get(key)?, "used_percent", read_at_ms)).collect();
    (!windows.is_empty()).then_some(windows)
}

/// The end of a file, from its first whole line.
fn read_tail(path: &Path, max: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    Some(if start == 0 { text.into_owned() } else { text.split_once('\n')?.1.to_string() })
}

fn subfolders_newest_first(dir: &Path) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    folders.sort_by(|a, b| b.cmp(a));
    folders
}

/// The logs written to most recently, newest first, looking only in the newest day
/// folders. (A session logs to the folder of the day it started.)
fn newest_rollouts(sessions: &Path) -> Vec<PathBuf> {
    let days = subfolders_newest_first(sessions)
        .into_iter()
        .flat_map(|year| subfolders_newest_first(&year))
        .flat_map(|month| subfolders_newest_first(&month))
        .take(CODEX_DAYS);
    let mut logs: Vec<(SystemTime, PathBuf)> = days
        .flat_map(|day| std::fs::read_dir(day).into_iter().flatten().flatten())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("rollout-"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    logs.sort_by(|a, b| b.0.cmp(&a.0));
    logs.into_iter().take(CODEX_LOGS).map(|(_, path)| path).collect()
}

fn codex_limit(sessions: &Path, now_ms: u64) -> Option<Limit> {
    let latest = newest_rollouts(sessions)
        .iter()
        .filter_map(|log| read_tail(log, CODEX_TAIL_BYTES))
        .find_map(|tail| tail.lines().rev().find_map(codex_reading))?;
    tightest(latest, now_ms)
}

// ---- Claude Code: what status lines report ------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct ClaudeLimits {
    five_hour: Option<Limit>,
    seven_day: Option<Limit>,
}

static CLAUDE: Mutex<ClaudeLimits> = Mutex::new(ClaudeLimits { five_hour: None, seven_day: None });

/// A session only knows the limits as of its own last reply, so an idle one can report
/// a lower number than another just did, or a window that's over: neither replaces
/// what's newer. A window the report leaves out stays until it resets.
fn merge(old: Option<Limit>, new: Option<Limit>) -> Option<Limit> {
    match (old, new) {
        (Some(old), Some(new)) if new.resets_at_ms < old.resets_at_ms => Some(old),
        (Some(old), Some(new)) if new.resets_at_ms == old.resets_at_ms && new.used_percent < old.used_percent => Some(old),
        (old, new) => new.or(old),
    }
}

/// What one status line report adds to what's known.
fn with_report(old: ClaudeLimits, report: &Value, now_ms: u64) -> ClaudeLimits {
    let reported = |key: &str| window(report.get("rate_limits")?.get(key)?, "used_percentage", now_ms);
    ClaudeLimits { five_hour: merge(old.five_hour, reported("five_hour")), seven_day: merge(old.seven_day, reported("seven_day")) }
}

/// A status line's JSON, from the hook channel. Only its limits are kept, in memory.
pub fn record_claude(body: &[u8]) {
    if let Ok(report) = serde_json::from_slice::<Value>(body) {
        let mut latest = CLAUDE.lock().unwrap();
        *latest = with_report(*latest, &report, now_ms());
    }
}

// ---- Claude Code: the opt-in status line ---------------------------------------------

/// What the wrapper needs to put back: the value of `statusLine` exactly as it was
/// written, or nothing if there wasn't one.
#[derive(Debug, Serialize, Deserialize)]
struct Backup {
    own: Option<String>,
}

/// `statusLine.command` while this is on.
fn wrapper_command(dir: &Path) -> String {
    let path = dir.join(WRAPPER.0).display().to_string();
    if cfg!(windows) {
        // Works whether Claude Code runs this in Git Bash or in PowerShell.
        format!("powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\"", path.replace('\\', "/"))
    } else {
        format!("sh '{}'", path.replace('\'', r"'\''"))
    }
}

/// Where `key`'s value sits in the text of a JSON object. Going by position means
/// everything else in the file stays as it was written (a `Value` would re-sort it).
fn member(text: &str, key: &str) -> Result<Option<Range<usize>>, String> {
    let members: HashMap<String, &RawValue> = serde_json::from_str(text).map_err(|e| format!("It isn't a JSON object ({e})."))?;
    Ok(members.get(key).map(|raw| {
        let start = raw.get().as_ptr() as usize - text.as_ptr() as usize;
        start..start + raw.get().len()
    }))
}

fn replace(text: &str, range: Range<usize>, with: &str) -> String {
    format!("{}{with}{}", &text[..range.start], &text[range.end..])
}

/// `text` with a new last member, indented the way Claude Code writes the file.
fn append_member(text: &str, key: &str, value: &str) -> String {
    let body = text[..text.rfind('}').unwrap_or(text.len())].trim_end();
    if body.ends_with('{') {
        format!("{body}\n  \"{key}\": {value}\n}}\n")
    } else {
        format!("{body},\n  \"{key}\": {value}{}", &text[body.len()..])
    }
}

/// `text` without `key`, whose value is at `value`.
fn remove_member(text: &str, key: &str, value: Range<usize>) -> Option<String> {
    let start = text[..value.start].rfind(&format!("\"{key}\""))?;
    let before = text[..start].trim_end();
    if let Some(before) = before.strip_suffix(',') {
        return Some(format!("{before}{}", &text[value.end..]));
    }
    let after = text[value.end..].trim_start();
    Some(match after.strip_prefix(',') {
        Some(after) => format!("{}{}", &text[..start], after.trim_start()),
        None => format!("{before}{after}"),
    })
}

/// Replaces the file in one step, so Claude Code never reads half of it. A settings
/// file symlinked from a dotfiles repo stays a symlink.
fn write_settings(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let temp = target.with_extension("json.littleguys");
    std::fs::write(&temp, text)?;
    let swapped = match std::fs::metadata(&target) {
        Ok(existing) => std::fs::set_permissions(&temp, existing.permissions()),
        Err(_) => Ok(()),
    }
    .and_then(|_| std::fs::rename(&temp, &target));
    if swapped.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    swapped
}

fn read_settings(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("{}\n".into()),
        Err(e) => Err(format!("Couldn't read {}: {e}", path.display())),
    }
}

/// Whether `statusLine` runs the wrapper in `dir` right now. That's the on/off state:
/// a status line you've set since is yours, and off.
fn is_on(text: &str, dir: &Path) -> bool {
    let Ok(Some(value)) = member(text, STATUS_LINE_KEY) else { return false };
    let Ok(line) = serde_json::from_str::<Value>(&text[value]) else { return false };
    line.get("command").and_then(Value::as_str) == Some(wrapper_command(dir).as_str())
}

fn turn_on(settings: &Path, dir: &Path) -> Result<(), String> {
    let text = read_settings(settings)?;
    let not_changed = |e: String| format!("littleguys left {} alone. {e}", settings.display());
    let existing = member(&text, STATUS_LINE_KEY).map_err(not_changed)?;
    if is_on(&text, dir) {
        return Ok(());
    }
    let wrapper = wrapper_command(dir);
    let (backup, own, updated) = match existing {
        Some(value) => {
            let original = &text[value.clone()];
            let parsed: Value = serde_json::from_str(original).map_err(|e| not_changed(e.to_string()))?;
            let Some(own) = parsed.get("command").and_then(Value::as_str) else {
                return Err(not_changed("Its status line isn't a command, so there's nothing to chain.".into()));
            };
            let command = member(original, "command").map_err(not_changed)?.expect("it was just read");
            let chained = replace(original, command, &json!(wrapper).to_string());
            (Backup { own: Some(original.to_string()) }, Some(own.to_string()), replace(&text, value, &chained))
        }
        None => {
            let line = serde_json::to_string_pretty(&json!({ "type": "command", "command": wrapper })).expect("it's plain JSON");
            (Backup { own: None }, None, append_member(&text, STATUS_LINE_KEY, &line.replace('\n', "\n  ")))
        }
    };
    let wrote = |e: std::io::Error| format!("Couldn't set up the status line: {e}");
    std::fs::create_dir_all(dir).map_err(wrote)?;
    std::fs::write(dir.join(WRAPPER.0), WRAPPER.1).map_err(wrote)?;
    if let Some(own) = own {
        std::fs::write(dir.join(OWN_FILE), own).map_err(wrote)?;
    }
    // The way back goes first: if littleguys stops here, nothing has changed yet.
    std::fs::write(dir.join(BACKUP_FILE), serde_json::to_vec(&backup).expect("it's plain JSON")).map_err(wrote)?;
    write_settings(settings, &updated).map_err(|e| format!("Couldn't update {}: {e}", settings.display()))
}

fn turn_off(settings: &Path, dir: &Path) -> Result<(), String> {
    let text = read_settings(settings)?;
    // Unreadable right now: the wrapper stays, or fixing the file would leave it dangling.
    let value = member(&text, STATUS_LINE_KEY).map_err(|e| format!("littleguys left {} alone. {e}", settings.display()))?;
    if let (Some(value), true) = (value, is_on(&text, dir)) {
        let backup: Option<Backup> = std::fs::read(dir.join(BACKUP_FILE)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let restored = match backup.and_then(|b| b.own) {
            Some(own) => replace(&text, value, &own),
            None => remove_member(&text, STATUS_LINE_KEY, value).ok_or("Couldn't find the status line to take out.")?,
        };
        write_settings(settings, &restored).map_err(|e| format!("Couldn't update {}: {e}", settings.display()))?;
    }
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

fn settings_path() -> PathBuf {
    claudecode::claude_home().join("settings.json")
}

/// Turns the chained status line on or off in Claude Code's user settings.
pub fn set_status_line(data_dir: &Path, on: bool) -> Result<(), String> {
    let dir = data_dir.join(STATUS_LINE_DIR);
    if on {
        turn_on(&settings_path(), &dir)
    } else {
        turn_off(&settings_path(), &dir)
    }
}

/// Sets the status line (when `on` is given); resolves to whether it's on.
#[tauri::command(async)]
pub fn claude_status_line(app: AppHandle, on: Option<bool>) -> Result<bool, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    if let Some(on) = on {
        set_status_line(&data_dir, on)?;
    }
    let text = read_settings(&settings_path())?;
    Ok(is_on(&text, &data_dir.join(STATUS_LINE_DIR)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;
    const NOW: u64 = 1_800_000_000_000;

    fn limit(used_percent: f64, resets_at_ms: u64) -> Limit {
        Limit { used_percent, resets_at_ms, read_at_ms: NOW - HOUR }
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lg-limits-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn token_count(at: &str, rate_limits: Value) -> String {
        json!({ "timestamp": at, "type": "event_msg", "payload": { "type": "token_count", "info": null, "rate_limits": rate_limits } }).to_string()
    }

    fn weekly(used: f64, resets_at_s: u64) -> Value {
        json!({ "limit_id": "codex", "limit_name": null, "primary": { "used_percent": used, "window_minutes": 10080, "resets_at": resets_at_s }, "secondary": null, "credits": null })
    }

    fn write_log(sessions: &Path, day: &str, name: &str, lines: &[String]) -> PathBuf {
        let folder = sessions.join(day);
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!("rollout-{name}.jsonl"));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn tightest_is_the_one_closest_to_running_out() {
        let five_hour = limit(41.0, NOW + 2 * HOUR);
        let seven_day = limit(63.0, NOW + 72 * HOUR);
        assert_eq!(tightest([five_hour, seven_day], NOW), Some(seven_day));
        assert_eq!(tightest([limit(90.0, NOW - 1), seven_day], NOW), Some(seven_day), "that window is over");
        assert_eq!(tightest([limit(50.0, NOW + HOUR), limit(50.0, NOW + 9 * HOUR)], NOW), Some(limit(50.0, NOW + 9 * HOUR)));
        assert_eq!(tightest([limit(90.0, NOW)], NOW), None, "reset this very moment");
        assert_eq!(tightest([], NOW), None);
    }

    #[test]
    fn reads_a_codex_token_count_line() {
        let line = token_count("2026-11-04T10:00:00.000Z", weekly(19.0, 1_800_000_000));
        let [window] = codex_reading(&line).unwrap()[..] else { panic!("one window") };
        assert_eq!(window.used_percent, 19.0);
        assert_eq!(window.resets_at_ms, 1_800_000_000_000);
        assert_eq!(window.read_at_ms, parse_iso_ms("2026-11-04T10:00:00.000Z").unwrap());

        let both = json!({ "limit_id": "codex", "primary": { "used_percent": 12.5, "resets_at": 1_800_000_000 }, "secondary": { "used_percent": 70, "resets_at": 1_800_100_000 } });
        assert_eq!(codex_reading(&token_count("2026-11-04T10:00:00Z", both)).unwrap().len(), 2);
    }

    #[test]
    fn skips_what_isnt_a_codex_reading() {
        let at = "2026-11-04T10:00:00Z";
        // No limits (an API key), or no time to go by.
        assert_eq!(codex_reading(&token_count(at, Value::Null)), None);
        assert_eq!(codex_reading(&token_count(at, json!({ "limit_id": "codex", "primary": null, "secondary": null }))), None);
        assert_eq!(codex_reading(&token_count(at, json!({ "limit_id": "codex", "primary": { "used_percent": 5.0 } }))), None);
        assert_eq!(codex_reading(&token_count("not a time", weekly(5.0, 1_800_000_000))), None);
        // Another metered limit, not the main one.
        let mut other = weekly(90.0, 1_800_000_000);
        other["limit_id"] = json!("codex_mini");
        assert_eq!(codex_reading(&token_count(at, other)), None);
        // Text that merely mentions it, and a line still being written.
        let chat = json!({ "timestamp": at, "type": "response_item", "payload": { "type": "message", "text": token_count(at, weekly(5.0, 1_800_000_000)) } });
        assert_eq!(codex_reading(&chat.to_string()), None);
        assert_eq!(codex_reading(&token_count(at, weekly(5.0, 1_800_000_000))[..40]), None);
    }

    #[test]
    fn codex_takes_the_newest_reading_in_the_newest_log() {
        let sessions = scratch();
        let resets = NOW / 1000 + 3600;
        write_log(&sessions, "2026/11/03", "older", &[token_count("2026-11-03T10:00:00Z", weekly(80.0, resets))]);
        let reply = json!({ "timestamp": "2026-11-04T10:00:00Z", "type": "response_item", "payload": {} }).to_string();
        // A subagent's log with no reading doesn't hide the one before it.
        let lines = [token_count("2026-11-04T09:00:00Z", weekly(20.0, resets)), token_count("2026-11-04T09:30:00Z", weekly(30.0, resets)), reply.clone()];
        let log = write_log(&sessions, "2026/11/04", "main", &lines);
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_log(&sessions, "2026/11/04", "subagent", &[reply]);

        let found = codex_limit(&sessions, NOW).unwrap();
        assert_eq!(found.used_percent, 30.0);
        assert_eq!(found.read_at_ms, parse_iso_ms("2026-11-04T09:30:00Z").unwrap());

        // A session that resumes logs to its old folder: going by last write finds it.
        std::fs::write(&log, token_count("2026-11-04T09:45:00Z", weekly(45.0, resets)) + "\n").unwrap();
        assert_eq!(codex_limit(&sessions, NOW).unwrap().used_percent, 45.0);
    }

    #[test]
    fn codex_hides_a_window_thats_over_and_never_falls_back_to_an_older_number() {
        let sessions = scratch();
        write_log(&sessions, "2026/11/02", "older", &[token_count("2026-11-02T10:00:00Z", weekly(80.0, NOW / 1000 + 3600))]);
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_log(&sessions, "2026/11/04", "newest", &[token_count("2026-11-04T10:00:00Z", weekly(10.0, NOW / 1000 - 1))]);
        assert_eq!(codex_limit(&sessions, NOW), None);
    }

    #[test]
    fn codex_without_logs_is_nothing() {
        assert_eq!(codex_limit(&scratch().join("nope"), NOW), None);
    }

    #[test]
    fn newest_rollouts_only_opens_the_newest_days() {
        let sessions = scratch();
        for day in 1..=CODEX_DAYS + 3 {
            write_log(&sessions, &format!("2026/10/{day:02}"), "a", &[String::new()]);
        }
        std::fs::write(sessions.join("2026/10/01/notes.txt"), "not a log").unwrap();
        let logs = newest_rollouts(&sessions);
        assert_eq!(logs.len(), CODEX_DAYS);
        assert!(logs.iter().all(|log| log.starts_with(sessions.join("2026/10")) && !log.starts_with(sessions.join("2026/10/03"))), "{logs:?}");
    }

    #[test]
    fn read_tail_starts_at_a_whole_line() {
        let path = scratch().join("log");
        std::fs::write(&path, "first line\nsecond\nthird\n").unwrap();
        assert_eq!(read_tail(&path, 4096).unwrap(), "first line\nsecond\nthird\n");
        assert_eq!(read_tail(&path, 16).unwrap(), "second\nthird\n");
        assert_eq!(read_tail(&path, 3).unwrap(), "", "only the end of a line");
        std::fs::write(&path, "one very long line with no end yet").unwrap();
        assert_eq!(read_tail(&path, 8), None);
    }

    fn report(five_hour: Option<(f64, u64)>, seven_day: Option<(f64, u64)>) -> Value {
        let mut limits = serde_json::Map::new();
        for (key, reading) in [("five_hour", five_hour), ("seven_day", seven_day)] {
            if let Some((used, resets_at)) = reading {
                limits.insert(key.into(), json!({ "used_percentage": used, "resets_at": resets_at }));
            }
        }
        json!({ "session_id": "s1", "cwd": "/Users/you/dev/shop-api", "rate_limits": limits })
    }

    #[test]
    fn claude_report_keeps_only_the_limits() {
        let at = NOW / 1000 + 3600;
        let got = with_report(ClaudeLimits::default(), &report(Some((23.5, at)), Some((41.2, at + 3600))), NOW);
        assert_eq!(got.five_hour, Some(Limit { used_percent: 23.5, resets_at_ms: at * 1000, read_at_ms: NOW }));
        assert_eq!(got.seven_day.unwrap().used_percent, 41.2);

        // Each window can be missing, and before a session's first reply they all are.
        let week_only = with_report(ClaudeLimits::default(), &report(None, Some((30.0, at))), NOW);
        assert_eq!((week_only.five_hour, week_only.seven_day.unwrap().used_percent), (None, 30.0));
        let first_run = json!({ "session_id": "s1", "cost": { "total_cost_usd": 0.0 } });
        assert_eq!(with_report(ClaudeLimits::default(), &first_run, NOW), ClaudeLimits::default());
        assert_eq!(with_report(ClaudeLimits::default(), &json!({ "rate_limits": null }), NOW), ClaudeLimits::default());
    }

    #[test]
    fn an_idle_session_cant_take_a_claude_reading_back() {
        let resets = NOW + 2 * HOUR;
        let latest = Some(limit(60.0, resets));
        assert_eq!(merge(latest, Some(limit(40.0, resets))), latest, "lower, same window");
        assert_eq!(merge(latest, Some(limit(90.0, resets - HOUR))), latest, "a window that's over");
        assert_eq!(merge(latest, None), latest);
        assert_eq!(merge(latest, Some(limit(70.0, resets))), Some(limit(70.0, resets)));
        assert_eq!(merge(latest, Some(limit(2.0, resets + 5 * HOUR))), Some(limit(2.0, resets + 5 * HOUR)), "a new window starts low");
        assert_eq!(merge(None, Some(limit(5.0, resets))), Some(limit(5.0, resets)));
    }

    #[test]
    fn claude_limit_goes_away_with_its_window() {
        let at = NOW / 1000 + 3600;
        let known = with_report(ClaudeLimits::default(), &report(Some((90.0, at)), Some((20.0, at + 86_400))), NOW);
        let windows = |k: ClaudeLimits| k.five_hour.into_iter().chain(k.seven_day);
        assert_eq!(tightest(windows(known), NOW).unwrap().used_percent, 90.0);
        assert_eq!(tightest(windows(known), NOW + 2 * HOUR).unwrap().used_percent, 20.0);
        assert_eq!(tightest(windows(known), NOW + 30 * HOUR), None);
    }

    const WRITTEN_BY_HAND: &str = r#"{
    "zebra": 1,
  "statusLine": {  "padding": 2, "command": "~/bin/line \"$HOME\"",
      "type": "command",   "refreshInterval": 5 },
  "apple" : [ 1,   2 ], "caf\u00e9": "keeps its escape",

  "hooks": {"Stop": []}
}
"#;

    fn paths() -> (PathBuf, PathBuf) {
        let root = scratch();
        (root.join("settings.json"), root.join(STATUS_LINE_DIR))
    }

    fn status_line(settings: &Path) -> Option<Value> {
        let settings: Value = serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
        settings.get(STATUS_LINE_KEY).cloned()
    }

    #[test]
    fn off_puts_an_existing_status_line_back_byte_for_byte() {
        let (settings, dir) = paths();
        std::fs::write(&settings, WRITTEN_BY_HAND).unwrap();

        turn_on(&settings, &dir).unwrap();
        let chained = std::fs::read_to_string(&settings).unwrap();
        assert!(is_on(&chained, &dir));
        let line = status_line(&settings).unwrap();
        assert_eq!(line["command"], wrapper_command(&dir));
        assert_eq!((&line["padding"], &line["refreshInterval"]), (&json!(2), &json!(5)), "the rest of its setup stays");
        assert_eq!(std::fs::read_to_string(dir.join(OWN_FILE)).unwrap(), "~/bin/line \"$HOME\"");
        // Only the command changed.
        assert_eq!(chained.replace(&json!(wrapper_command(&dir)).to_string(), r#""~/bin/line \"$HOME\"""#), WRITTEN_BY_HAND);

        turn_off(&settings, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), WRITTEN_BY_HAND);
        assert!(!dir.exists());
    }

    #[test]
    fn off_takes_out_the_status_line_it_added() {
        let (settings, dir) = paths();
        for original in ["{}\n", "{\n  \"model\": \"sonnet\"\n}\n", "{\n  \"zebra\": 1,\n  \"apple\": {\n    \"b\": 1,\n    \"a\": 2\n  }\n}"] {
            std::fs::write(&settings, original).unwrap();
            turn_on(&settings, &dir).unwrap();
            let line = status_line(&settings).unwrap();
            assert_eq!(line, json!({ "type": "command", "command": wrapper_command(&dir) }));
            assert!(!dir.join(OWN_FILE).exists());
            turn_off(&settings, &dir).unwrap();
            assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);
        }
    }

    #[test]
    fn on_with_no_settings_file_makes_one_and_off_clears_it() {
        let (settings, dir) = paths();
        turn_on(&settings, &dir).unwrap();
        assert!(is_on(&std::fs::read_to_string(&settings).unwrap(), &dir));
        turn_off(&settings, &dir).unwrap();
        assert_eq!(status_line(&settings), None);
    }

    #[test]
    fn keys_added_while_on_survive_turning_off() {
        let (settings, dir) = paths();
        std::fs::write(&settings, "{\n  \"statusLine\": {\n    \"command\": \"mine\",\n    \"type\": \"command\"\n  },\n  \"model\": \"sonnet\"\n}\n").unwrap();
        turn_on(&settings, &dir).unwrap();
        let edited = std::fs::read_to_string(&settings).unwrap().replace("\"sonnet\"", "\"opus\"");
        std::fs::write(&settings, &edited).unwrap();
        turn_off(&settings, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{\n  \"statusLine\": {\n    \"command\": \"mine\",\n    \"type\": \"command\"\n  },\n  \"model\": \"opus\"\n}\n");

        // The one it added, with other keys coming after it by now.
        std::fs::write(&settings, "{\n  \"model\": \"sonnet\"\n}\n").unwrap();
        turn_on(&settings, &dir).unwrap();
        let edited = std::fs::read_to_string(&settings).unwrap().replace("\n}\n", ",\n  \"effortLevel\": \"high\"\n}\n");
        std::fs::write(&settings, &edited).unwrap();
        turn_off(&settings, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{\n  \"model\": \"sonnet\",\n  \"effortLevel\": \"high\"\n}\n");
    }

    #[test]
    fn on_twice_never_chains_the_wrapper_to_itself() {
        let (settings, dir) = paths();
        std::fs::write(&settings, WRITTEN_BY_HAND).unwrap();
        turn_on(&settings, &dir).unwrap();
        turn_on(&settings, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(OWN_FILE)).unwrap(), "~/bin/line \"$HOME\"");
        turn_off(&settings, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), WRITTEN_BY_HAND);
    }

    #[test]
    fn off_leaves_a_status_line_set_since_alone() {
        let (settings, dir) = paths();
        std::fs::write(&settings, WRITTEN_BY_HAND).unwrap();
        turn_on(&settings, &dir).unwrap();
        let theirs = "{\n  \"statusLine\": { \"type\": \"command\", \"command\": \"their-new-line\" }\n}\n";
        std::fs::write(&settings, theirs).unwrap();
        assert!(!is_on(theirs, &dir));
        turn_off(&settings, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), theirs);
        assert!(!dir.exists());
    }

    #[test]
    fn off_without_its_backup_still_stops_running_the_wrapper() {
        let (settings, dir) = paths();
        std::fs::write(&settings, "{\n  \"model\": \"sonnet\"\n}\n").unwrap();
        turn_on(&settings, &dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        turn_off(&settings, &dir).unwrap();
        assert_eq!(status_line(&settings), None);
    }

    #[test]
    fn refuses_to_touch_settings_it_cant_read() {
        for broken in ["{ not json", "[1, 2]", "{\"statusLine\": {\"type\": \"static\"}}"] {
            let (settings, dir) = paths();
            std::fs::write(&settings, broken).unwrap();
            assert!(turn_on(&settings, &dir).is_err(), "{broken}");
            assert_eq!(std::fs::read_to_string(&settings).unwrap(), broken);
            assert!(!dir.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_stays_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let (settings, dir) = paths();
        let dotfiles = scratch().join("claude-settings.json");
        std::fs::write(&dotfiles, "{\n  \"model\": \"sonnet\"\n}\n").unwrap();
        std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&dotfiles, &settings).unwrap();
        turn_on(&settings, &dir).unwrap();
        assert!(std::fs::symlink_metadata(&settings).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::metadata(&dotfiles).unwrap().permissions().mode() & 0o777, 0o640);
        assert!(is_on(&std::fs::read_to_string(&dotfiles).unwrap(), &dir));
    }

    /// A data dir short enough for a unix socket (macOS' temp dir isn't).
    #[cfg(unix)]
    fn short_data_dir(name: &str) -> PathBuf {
        let dir = Path::new("/tmp").join(format!("lg{}", &uuid::Uuid::new_v4().simple().to_string()[..8])).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Turns the status line on in a data dir, then runs it the way Claude Code does.
    #[cfg(unix)]
    fn run_status_line(data_dir: &Path, original: Option<&str>, input: &str) -> std::process::Output {
        use std::io::Write as _;
        let settings = data_dir.join("settings.json");
        let line = original.map_or(String::new(), |c| format!("\"statusLine\": {}", json!({ "type": "command", "command": c })));
        std::fs::write(&settings, format!("{{{line}}}")).unwrap();
        turn_on(&settings, &data_dir.join(STATUS_LINE_DIR)).unwrap();
        let command = status_line(&settings).unwrap()["command"].as_str().unwrap().to_string();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapper_reports_then_runs_your_status_line() {
        use std::io::{Read as _, Write as _};
        // A space, as in Application Support, and a quote for good measure.
        let data_dir = short_data_dir("App's Data");
        let listener = std::os::unix::net::UnixListener::bind(claudecode::socket_path(&data_dir)).unwrap();
        let app = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            while !request.ends_with(b"}\n") {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0, "the report stopped short");
                request.extend_from_slice(&chunk[..n]);
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            String::from_utf8(request).unwrap()
        });

        let input = "{\"rate_limits\":{\"seven_day\":{\"used_percentage\":30,\"resets_at\":1800000000}}}\n";
        let out = run_status_line(&data_dir, Some("cat; echo mine; exit 3"), input);
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{input}mine\n"), "your status line got the same input");
        assert_eq!(out.status.code(), Some(3), "and its exit code counts");

        let request = app.join().unwrap();
        assert!(request.starts_with(&format!("POST {CLAUDE_REPORT_PATH} ")), "{request}");
        assert!(request.ends_with(input), "{request}");
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapper_runs_your_status_line_when_littleguys_isnt_running() {
        let data_dir = scratch();
        let out = run_status_line(&data_dir, Some("cat"), "{\"a\":1}\n\n");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "{\"a\":1}\n\n", "trailing newlines and all");
        assert!(out.status.success());

        // A dead socket, as after a crash.
        let dead = short_data_dir("data");
        drop(std::os::unix::net::UnixListener::bind(claudecode::socket_path(&dead)).unwrap());
        let started = std::time::Instant::now();
        assert_eq!(String::from_utf8_lossy(&run_status_line(&dead, Some("cat"), "{}").stdout), "{}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapper_prints_nothing_when_you_have_no_status_line() {
        let out = run_status_line(&scratch(), None, "{}");
        assert!(out.status.success() && out.stdout.is_empty());
    }

    /// Runs the wrapper the way Claude Code does without Git Bash.
    #[cfg(windows)]
    #[test]
    fn the_powershell_wrapper_runs_your_status_line() {
        use std::io::Write as _;
        let data_dir = scratch();
        let settings = data_dir.join("settings.json");
        let dir = data_dir.join(STATUS_LINE_DIR);
        let own = "[Console]::Out.Write('mine: ' + [Console]::In.ReadToEnd())";
        std::fs::write(&settings, json!({ "statusLine": { "type": "command", "command": own } }).to_string()).unwrap();
        turn_on(&settings, &dir).unwrap();
        assert!(wrapper_command(&dir).contains('/') && !wrapper_command(&dir).contains('\\'), "Git Bash would eat a backslash");
        let mut child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(dir.join(WRAPPER.0))
            .env("CLAUDE_CODE_GIT_BASH_PATH", "C:/nowhere/bash.exe")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"{\"a\":1}").unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "mine: {\"a\":1}");
    }

    /// What the readers make of this machine's real files, printed, not asserted:
    /// `cargo test --lib limits::tests::real_files -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_files() {
        let sessions = codex_sessions();
        let logs = newest_rollouts(&sessions);
        println!("{} logs to try in {}", logs.len(), sessions.display());
        for log in &logs {
            let tail = read_tail(log, CODEX_TAIL_BYTES).unwrap_or_default();
            let reading = tail.lines().rev().find_map(codex_reading);
            println!("{}: {} bytes, {reading:?}", log.file_name().unwrap().to_string_lossy(), tail.len());
        }
        let started = std::time::Instant::now();
        let limit = read("codex", now_ms());
        println!("codex: {limit:?} in {:?}", started.elapsed());
    }
}
