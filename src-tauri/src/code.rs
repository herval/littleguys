//! Coder mode: where a thread's work stands in its git repo (branch, what changed, its pull
//! request), for threads that work in a folder on this machine. See docs/CODER-MODE.md.
//!
//! It only reads. Every git command here reads, with `--no-optional-locks`, so a status
//! never takes `index.lock` while an agent commits. Commits, pushes and pull requests are
//! the agent's to make when you ask it to. Nothing runs while coder mode is off, and a
//! folder with no `.git` above it is answered from a file check alone.
//!
//! Calls, answered by [`crate::runtime::Runtime::call`] for runtimes with `capabilities.code`:
//! - `code.status {sessionKey}` → `{repo: null, why: off|not-a-repo|no-git}`, else
//!   `{repo: {root, name, mainRoot?, worktree}, branch?, detached, head, base?: {ref, sha},
//!   upstream?, ahead, behind, commits, dirty, changes: {files, added, removed, untracked}, conflicted}`.
//!   `changes` is everything since the branch left its base (on the base branch itself, since
//!   its upstream), committed or not, untracked files included; `dirty` counts the files not
//!   committed yet and `commits` the commits since the base.
//! - `code.diff {sessionKey, path?}` → `{base, files: [{path, oldPath?, status, added, removed,
//!   binary}], patch?, truncated}`: `patch` is the unified diff (one file's with `path`).
//! - `code.pr {sessionKey}` → `{pr: null | {number, url, title, state, draft, checks, failing,
//!   review?}}`, or `{unavailable: off|no-gh|not-github|logged-out}`. Through `gh`, only with
//!   the pull request switch on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::gateway::client::RpcError;

/// Settings → Coder mode, kept in prefs.json with the app's other prefs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CoderPrefs {
    /// The switch: off, coder mode shows nothing and runs nothing.
    pub on: bool,
    /// "Start chats in": folders always offered when a thread picks where it works.
    pub folders: Vec<String>,
    /// The Changes panel opens on its own beside a thread with changes.
    pub panel: bool,
    /// New threads can start in their own worktree (`claude -w`, `codex --worktree`).
    pub worktrees: bool,
    /// Pull requests and checks, through `gh`.
    pub prs: bool,
    /// The one-time "Turn on Coder mode" line was answered.
    pub offered: bool,
    /// Coding threads get a button that opens a terminal in their folder (terminal.rs).
    pub terminal: bool,
    /// The terminal it opens: a `.app` on a Mac, `wt`, `pwsh`, `powershell` or `cmd` on a PC.
    /// None for the system's own.
    pub terminal_app: Option<String>,
}

impl Default for CoderPrefs {
    fn default() -> Self {
        OFF
    }
}

const OFF: CoderPrefs = CoderPrefs {
    on: false,
    folders: Vec::new(),
    panel: true,
    worktrees: false,
    prs: false,
    offered: false,
    terminal: true,
    terminal_app: None,
};

static PREFS: RwLock<CoderPrefs> = RwLock::new(OFF);

pub fn prefs() -> CoderPrefs {
    PREFS.read().unwrap().clone()
}

/// Called with what prefs.json says at startup, and on every change.
pub fn set_prefs(prefs: CoderPrefs) {
    *PREFS.write().unwrap() = prefs;
    // What was read under the old settings may not hold under the new ones.
    STATUS.lock().unwrap().clear();
    PRS.lock().unwrap().clear();
}

/// Whether new threads may start in their own worktree right now.
pub fn worktrees_allowed() -> bool {
    let p = PREFS.read().unwrap();
    p.on && p.worktrees
}

/// `code.<op>` for a thread working in `folder`.
pub async fn call(op: &str, folder: Option<PathBuf>, params: &Value) -> Result<Value, RpcError> {
    let prefs = prefs();
    let folder = folder.ok_or_else(|| RpcError::new("UNSUPPORTED", "This thread's folder isn't on this computer."))?;
    match op {
        "status" => status(&folder, &prefs).await,
        "diff" => diff(&folder, params.get("path").and_then(Value::as_str), &prefs).await,
        "pr" => pr(&folder, &prefs).await,
        _ => Err(crate::runtime::unsupported(&format!("code.{op}"))),
    }
}

/// What the Coder mode tab needs to know: whether git and `gh` are here.
pub fn tools() -> Value {
    json!({ "git": git_program().is_some(), "gh": crate::platform::find_program("gh").is_some() })
}

// ---- where git is -----------------------------------------------------------------------

/// The repo `folder` is in, by looking for `.git` (a folder, or a file in a worktree or
/// submodule) in it and above it. No process runs.
pub fn repo_root(folder: &Path) -> Option<PathBuf> {
    folder.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// The `git` to run, or None. On a Mac without the developer tools, `/usr/bin/git` is a
/// stub that pops up an installer, so it only counts when there's a real git behind it.
fn git_program() -> Option<PathBuf> {
    static FOUND: Mutex<Option<(Instant, Option<PathBuf>)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap();
    if let Some((at, git)) = found.as_ref() {
        // Looked up once; a missing git is looked for again now and then, in case it got installed.
        if git.is_some() || at.elapsed() < Duration::from_secs(300) {
            return git.clone();
        }
    }
    let git = crate::platform::find_program("git").filter(|p| real_git(p));
    *found = Some((Instant::now(), git.clone()));
    git
}

#[cfg(target_os = "macos")]
fn real_git(path: &Path) -> bool {
    if path != Path::new("/usr/bin/git") {
        return true;
    }
    let out = crate::platform::std_command("/usr/bin/xcode-select").arg("-p").stdin(Stdio::null()).output();
    out.ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("usr/bin/git"))
        .is_some_and(|g| g.exists())
}

#[cfg(not(target_os = "macos"))]
fn real_git(_path: &Path) -> bool {
    true
}

const GIT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
enum GitError {
    NoGit,
    NotARepo,
    Failed(String),
}

impl From<GitError> for RpcError {
    fn from(e: GitError) -> Self {
        match e {
            GitError::NoGit => RpcError::new("UNAVAILABLE", "Git isn't installed on this computer."),
            GitError::NotARepo => RpcError::new("NOT_FOUND", "This folder isn't in a git repo."),
            GitError::Failed(message) => RpcError::new("GIT", message),
        }
    }
}

/// `git <args>` in `dir`, read-only. Its output, or why not.
async fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let program = git_program().ok_or(GitError::NoGit)?;
    let mut cmd = crate::platform::command(&program);
    cmd.arg("--no-optional-locks")
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| GitError::Failed(format!("git {} took too long", args.first().unwrap_or(&""))))?
        .map_err(|e| GitError::Failed(e.to_string()))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("not a git repository") {
        return Err(GitError::NotARepo);
    }
    Err(GitError::Failed(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("git failed").trim().to_string()))
}

/// Like [`git`], for answers that may fairly be empty (a ref that doesn't exist).
async fn git_opt(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).await.ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

// ---- status -----------------------------------------------------------------------------

/// What [`status`] read about a repo, kept a moment: threads share folders, and a turn's
/// end refreshes every row in one go.
struct Cached {
    at: Instant,
    value: Value,
}

type Slot = Arc<tokio::sync::Mutex<Option<Cached>>>;

const STATUS_FOR: Duration = Duration::from_secs(2);
static STATUS: LazyLock<Mutex<HashMap<PathBuf, Slot>>> = LazyLock::new(Default::default);

fn slot(map: &Mutex<HashMap<PathBuf, Slot>>, key: &Path) -> Slot {
    map.lock().unwrap().entry(key.to_path_buf()).or_default().clone()
}

/// The comparison point for a repo: its default branch (the remote's, else a local `main`
/// or `master`), kept a minute since it hardly changes.
struct Base {
    at: Instant,
    /// e.g. `origin/main`, or `main`.
    name: Option<String>,
    /// The repo's own folder, for a worktree.
    main_root: Option<PathBuf>,
}

static BASES: LazyLock<Mutex<HashMap<PathBuf, Arc<Base>>>> = LazyLock::new(Default::default);

async fn base_of(root: &Path) -> Result<Arc<Base>, GitError> {
    if let Some(b) = BASES.lock().unwrap().get(root).filter(|b| b.at.elapsed() < Duration::from_secs(60)) {
        return Ok(b.clone());
    }
    let dirs = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir", "--show-toplevel"]).await?;
    let mut lines = dirs.lines().map(str::trim);
    let common = lines.next().map(native_path);
    let top = lines.next().map(native_path);
    // A worktree's git dir lives in the main repo's `.git`; its parent is the repo.
    let main_root = match (common, top) {
        (Some(common), Some(top)) if common.file_name().is_some_and(|n| n == ".git") => {
            common.parent().map(Path::to_path_buf).filter(|main| !same_dir(main, &top))
        }
        _ => None,
    };
    let name = match git_opt(root, &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"]).await {
        Some(name) => Some(name),
        None => {
            let refs = git_opt(
                root,
                &["for-each-ref", "--format=%(refname:short)", "refs/remotes/origin/main", "refs/remotes/origin/master", "refs/heads/main", "refs/heads/master"],
            )
            .await
            .unwrap_or_default();
            let have: Vec<&str> = refs.lines().map(str::trim).collect();
            ["origin/main", "origin/master", "main", "master"].into_iter().find(|r| have.contains(r)).map(str::to_string)
        }
    };
    let base = Arc::new(Base { at: Instant::now(), name, main_root });
    BASES.lock().unwrap().insert(root.to_path_buf(), base.clone());
    Ok(base)
}

/// A path git printed, in this system's own form: git on Windows writes `C:/a/b`.
fn native_path(printed: &str) -> PathBuf {
    Path::new(printed).components().collect()
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// The tree an empty repo's changes are counted from.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

async fn status(folder: &Path, prefs: &CoderPrefs) -> Result<Value, RpcError> {
    if !prefs.on {
        return Ok(json!({ "repo": null, "why": "off" }));
    }
    let Some(root) = repo_root(folder) else {
        return Ok(json!({ "repo": null, "why": "not-a-repo" }));
    };
    let slot = slot(&STATUS, &root);
    let mut cached = slot.lock().await;
    if let Some(c) = cached.as_ref().filter(|c| c.at.elapsed() < STATUS_FOR) {
        return Ok(c.value.clone());
    }
    let value = match read_status(&root).await {
        Ok(value) => value,
        Err(GitError::NoGit) => json!({ "repo": null, "why": "no-git" }),
        Err(GitError::NotARepo) => json!({ "repo": null, "why": "not-a-repo" }),
        Err(e) => return Err(e.into()),
    };
    *cached = Some(Cached { at: Instant::now(), value: value.clone() });
    Ok(value)
}

/// What `git status --porcelain=v2 --branch -z` says.
#[derive(Debug, Default, PartialEq)]
struct Porcelain {
    head: Option<String>,
    branch: Option<String>,
    upstream: Option<String>,
    ahead: u64,
    behind: u64,
    /// Tracked files with changes not committed.
    changed: Vec<String>,
    untracked: Vec<String>,
    conflicted: u64,
}

fn parse_porcelain(out: &str) -> Porcelain {
    let mut p = Porcelain::default();
    let mut tokens = out.split('\0').filter(|t| !t.is_empty());
    while let Some(t) = tokens.next() {
        if let Some(header) = t.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => p.head = Some(value.to_string()),
                "branch.head" if value != "(detached)" => p.branch = Some(value.to_string()),
                "branch.upstream" => p.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+') {
                            p.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            p.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        match t.split_once(' ') {
            // `1 XY sub mH mI mW hH hI path`
            Some(("1", rest)) => p.changed.extend(rest.splitn(8, ' ').nth(7).map(str::to_string)),
            // `2 XY sub mH mI mW hH hI Xscore path`, then the original path
            Some(("2", rest)) => {
                p.changed.extend(rest.splitn(9, ' ').nth(8).map(str::to_string));
                tokens.next();
            }
            Some(("u", rest)) => {
                p.conflicted += 1;
                p.changed.extend(rest.splitn(10, ' ').nth(9).map(str::to_string));
            }
            Some(("?", path)) => p.untracked.push(path.to_string()),
            _ => {}
        }
    }
    p
}

/// One file of `git diff --numstat -z`: lines added and removed, None for binary.
#[derive(Debug, PartialEq)]
struct NumStat {
    path: String,
    old_path: Option<String>,
    added: Option<u64>,
    removed: Option<u64>,
}

fn parse_numstat(out: &str) -> Vec<NumStat> {
    let mut stats = Vec::new();
    let mut tokens = out.split('\0');
    while let Some(t) = tokens.next() {
        let mut parts = t.splitn(3, '\t');
        let (Some(a), Some(r), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let (path, old_path) = if path.is_empty() {
            // A rename: the old path, then the new one.
            let old = tokens.next().unwrap_or_default().to_string();
            (tokens.next().unwrap_or_default().to_string(), Some(old))
        } else {
            (path.to_string(), None)
        };
        stats.push(NumStat { path, old_path, added: a.parse().ok(), removed: r.parse().ok() });
    }
    stats
}

/// Where a branch's changes are counted from: its base branch, or on that branch itself
/// its upstream (what isn't pushed), or with neither its own HEAD (what isn't committed).
fn compare_ref(branch: Option<&str>, upstream: Option<&str>, base: Option<&str>) -> String {
    let on_base = matches!((branch, base), (Some(b), Some(base)) if base == b || base.rsplit_once('/').is_some_and(|(_, short)| short == b));
    match (on_base, upstream, base) {
        (true, Some(up), _) => up.to_string(),
        (true, None, _) | (false, _, None) => "HEAD".to_string(),
        (false, _, Some(base)) => base.to_string(),
    }
}

async fn read_status(root: &Path) -> Result<Value, GitError> {
    let porcelain = parse_porcelain(&git(root, &["status", "--porcelain=v2", "--branch", "-z"]).await?);
    let base = base_of(root).await?;
    let compare = compare_ref(porcelain.branch.as_deref(), porcelain.upstream.as_deref(), base.name.as_deref());
    let since = match &porcelain.head {
        None => EMPTY_TREE.to_string(),
        Some(head) if compare == "HEAD" => head.clone(),
        Some(head) => git_opt(root, &["merge-base", "HEAD", &compare]).await.unwrap_or_else(|| head.clone()),
    };
    let stats = parse_numstat(&git(root, &["diff", "--numstat", "-z", "-M", &since]).await?);
    let commits = match &porcelain.head {
        Some(head) if *head != since => git_opt(root, &["rev-list", "--count", &format!("{since}..HEAD")]).await.and_then(|n| n.parse().ok()).unwrap_or(0),
        _ => 0u64,
    };
    let untracked = untracked_files(root, &porcelain.untracked);
    let added = stats.iter().filter_map(|s| s.added).sum::<u64>() + untracked.iter().map(|(_, lines)| lines).sum::<u64>();
    let removed = stats.iter().filter_map(|s| s.removed).sum::<u64>();
    let name = base.main_root.as_deref().unwrap_or(root).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(json!({
        "repo": {
            "root": root.display().to_string(),
            "name": name,
            "mainRoot": base.main_root.as_ref().map(|p| p.display().to_string()),
            "worktree": base.main_root.is_some(),
        },
        "branch": porcelain.branch,
        "detached": porcelain.branch.is_none() && porcelain.head.is_some(),
        "head": porcelain.head,
        // `ref` is what the changes are counted from: a branch, or "HEAD" for the last commit.
        "base": { "ref": compare, "sha": since },
        "upstream": porcelain.upstream,
        "ahead": porcelain.ahead,
        "behind": porcelain.behind,
        "commits": commits,
        "dirty": porcelain.changed.len() + untracked.len(),
        "changes": {
            "files": stats.len() + untracked.len(),
            "added": added,
            "removed": removed,
            "untracked": untracked.len(),
        },
        "conflicted": porcelain.conflicted,
    }))
}

/// Untracked files under `root` (folders `git status` names whole are walked), each with
/// its line count. At most [`MAX_UNTRACKED`] of them.
fn untracked_files(root: &Path, listed: &[String]) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut stack: Vec<String> = listed.iter().rev().cloned().collect();
    while let Some(rel) = stack.pop() {
        if out.len() >= MAX_UNTRACKED {
            break;
        }
        let path = root.join(&rel);
        if rel.ends_with('/') {
            let Ok(entries) = std::fs::read_dir(&path) else { continue };
            let mut names: Vec<String> = entries
                .flatten()
                .map(|e| {
                    let dir = e.file_type().is_ok_and(|t| t.is_dir());
                    format!("{rel}{}{}", e.file_name().to_string_lossy(), if dir { "/" } else { "" })
                })
                .collect();
            names.sort();
            stack.extend(names.into_iter().rev());
            continue;
        }
        out.push((rel, text_of(&path).map(|t| line_count(&t)).unwrap_or(0)));
    }
    out
}

const MAX_UNTRACKED: usize = 500;
/// Untracked files bigger than this aren't read (for their line count or their patch).
const MAX_TEXT: u64 = 512 * 1024;

/// A small text file's contents; None for big or binary ones.
fn text_of(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_TEXT {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn line_count(text: &str) -> u64 {
    let n = text.matches('\n').count() as u64;
    if text.is_empty() || text.ends_with('\n') {
        n
    } else {
        n + 1
    }
}

// ---- diff -------------------------------------------------------------------------------

/// Past this, `code.diff` without `path` lists the files but leaves the patch out.
const MAX_PATCH: usize = 256 * 1024;
/// One file's patch is cut here.
const MAX_FILE_PATCH: usize = 1024 * 1024;

async fn diff(folder: &Path, path: Option<&str>, prefs: &CoderPrefs) -> Result<Value, RpcError> {
    let st = status(folder, prefs).await?;
    let Some(root) = st.pointer("/repo/root").and_then(Value::as_str).map(PathBuf::from) else {
        return Ok(json!({ "files": [], "why": st.get("why") }));
    };
    let since = st.pointer("/base/sha").and_then(Value::as_str).or_else(|| st.get("head").and_then(Value::as_str)).unwrap_or(EMPTY_TREE).to_string();
    let path = path.map(str::trim).filter(|p| !p.is_empty() && !p.contains("..")).map(str::to_string);
    let mut args: Vec<String> = vec!["diff".into(), "--name-status".into(), "-z".into(), "-M".into(), since.clone()];
    if let Some(p) = &path {
        args.extend(["--".into(), p.clone()]);
    }
    let names = git(&root, &args.iter().map(String::as_str).collect::<Vec<_>>()).await.map_err(RpcError::from)?;
    args[1] = "--numstat".into();
    let stats = parse_numstat(&git(&root, &args.iter().map(String::as_str).collect::<Vec<_>>()).await.map_err(RpcError::from)?);
    let mut files: Vec<Value> = parse_name_status(&names)
        .into_iter()
        .map(|(status, path, old_path)| {
            let stat = stats.iter().find(|s| s.path == path);
            json!({
                "path": path,
                "oldPath": old_path,
                "status": status,
                "added": stat.and_then(|s| s.added).unwrap_or(0),
                "removed": stat.and_then(|s| s.removed).unwrap_or(0),
                "binary": stat.is_some_and(|s| s.added.is_none()),
            })
        })
        .collect();

    let mut untracked_args = vec!["ls-files", "--others", "--exclude-standard", "-z"];
    if let Some(p) = &path {
        untracked_args.extend(["--", p.as_str()]);
    }
    let untracked: Vec<String> = git(&root, &untracked_args)
        .await
        .map(|out| out.split('\0').filter(|p| !p.is_empty()).take(MAX_UNTRACKED).map(str::to_string).collect())
        .unwrap_or_default();
    let mut created = String::new();
    for rel in &untracked {
        let text = text_of(&root.join(rel));
        files.push(json!({
            "path": rel,
            "status": "untracked",
            "added": text.as_deref().map(line_count).unwrap_or(0),
            "removed": 0,
            "binary": text.is_none(),
        }));
        if let Some(text) = text {
            created.push_str(&created_patch(rel, &text));
        }
    }

    let cap = if path.is_some() { MAX_FILE_PATCH } else { MAX_PATCH };
    args[1] = "--no-color".into();
    args.insert(2, "--no-ext-diff".into());
    args.retain(|a| a != "-z");
    let mut patch = git(&root, &args.iter().map(String::as_str).collect::<Vec<_>>()).await.map_err(RpcError::from)?;
    patch.push_str(&created);
    let truncated = patch.len() > cap;
    Ok(json!({
        "base": st.get("base"),
        "files": files,
        "patch": (!truncated).then_some(patch),
        "truncated": truncated,
    }))
}

/// `git diff --name-status -z`: (status, path, old path for a rename or copy).
fn parse_name_status(out: &str) -> Vec<(&'static str, String, Option<String>)> {
    let mut rows = Vec::new();
    let mut tokens = out.split('\0').filter(|t| !t.is_empty());
    while let Some(code) = tokens.next() {
        let status = match code.chars().next() {
            Some('A') => "added",
            Some('D') => "deleted",
            Some('R') => "renamed",
            Some('C') => "copied",
            _ => "modified",
        };
        if matches!(status, "renamed" | "copied") {
            let old = tokens.next().unwrap_or_default().to_string();
            let new = tokens.next().unwrap_or_default().to_string();
            rows.push((status, new, Some(old)));
        } else if let Some(path) = tokens.next() {
            rows.push((status, path.to_string(), None));
        }
    }
    rows
}

/// A new file as a unified diff, the way `git diff` shows one.
fn created_patch(path: &str, text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = format!("diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n");
    if lines.is_empty() {
        return out;
    }
    out.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
    for line in lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ---- pull requests ----------------------------------------------------------------------

/// `gh` is asked about a repo at most this often.
const PR_FOR: Duration = Duration::from_secs(60);
static PRS: LazyLock<Mutex<HashMap<PathBuf, Slot>>> = LazyLock::new(Default::default);

async fn pr(folder: &Path, prefs: &CoderPrefs) -> Result<Value, RpcError> {
    if !prefs.on || !prefs.prs {
        return Ok(json!({ "unavailable": "off" }));
    }
    let Some(root) = repo_root(folder) else {
        return Ok(json!({ "pr": null }));
    };
    let Some(gh) = crate::platform::find_program("gh") else {
        return Ok(json!({ "unavailable": "no-gh" }));
    };
    let slot = slot(&PRS, &root);
    let mut cached = slot.lock().await;
    if let Some(c) = cached.as_ref().filter(|c| c.at.elapsed() < PR_FOR) {
        return Ok(c.value.clone());
    }
    let value = read_pr(&gh, &root).await;
    *cached = Some(Cached { at: Instant::now(), value: value.clone() });
    Ok(value)
}

async fn read_pr(gh: &Path, root: &Path) -> Value {
    let mut cmd = crate::platform::command(gh);
    cmd.args(["pr", "view", "--json", "number,url,title,state,isDraft,reviewDecision,statusCheckRollup"])
        .current_dir(root)
        .env("PATH", crate::platform::login_path())
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(Duration::from_secs(20), cmd.output()).await {
        Ok(Ok(out)) => out,
        _ => return json!({ "pr": null }),
    };
    if !out.status.success() {
        return json!(pr_error(&String::from_utf8_lossy(&out.stderr)));
    }
    serde_json::from_slice::<Value>(&out.stdout).map(|v| json!({ "pr": pr_summary(&v) })).unwrap_or_else(|_| json!({ "pr": null }))
}

/// What a failed `gh pr view` means.
fn pr_error(stderr: &str) -> Value {
    let e = stderr.to_ascii_lowercase();
    if e.contains("gh auth login") || e.contains("not logged in") || e.contains("authentication") {
        json!({ "unavailable": "logged-out" })
    } else if e.contains("none of the git remotes") || e.contains("not a git repository") || e.contains("no git remotes") {
        json!({ "unavailable": "not-github" })
    } else {
        // "no pull requests found for branch …", or anything else: no PR to show.
        json!({ "pr": null })
    }
}

/// `gh pr view --json …` in the shape `code.pr` answers with.
fn pr_summary(v: &Value) -> Value {
    let checks = v.get("statusCheckRollup").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut failing = Vec::new();
    let mut running = false;
    for c in &checks {
        let name = c.get("name").or_else(|| c.get("context")).and_then(Value::as_str).unwrap_or("check").to_string();
        // Check runs have a status and, once done, a conclusion; commit statuses a state.
        let state = c.get("conclusion").and_then(Value::as_str).filter(|s| !s.is_empty()).or_else(|| c.get("state").and_then(Value::as_str)).unwrap_or("");
        let status = c.get("status").and_then(Value::as_str).unwrap_or("COMPLETED");
        match state.to_ascii_uppercase().as_str() {
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => failing.push(name),
            "PENDING" | "EXPECTED" => running = true,
            "" if status != "COMPLETED" => running = true,
            _ => {}
        }
    }
    let checks_state = if checks.is_empty() {
        "none"
    } else if !failing.is_empty() {
        "failing"
    } else if running {
        "running"
    } else {
        "passing"
    };
    let review = match v.get("reviewDecision").and_then(Value::as_str).unwrap_or("") {
        "APPROVED" => Some("approved"),
        "CHANGES_REQUESTED" => Some("changes"),
        "REVIEW_REQUIRED" => Some("pending"),
        _ => None,
    };
    json!({
        "number": v.get("number"),
        "url": v.get("url"),
        "title": v.get("title"),
        "state": v.get("state").and_then(Value::as_str).map(str::to_ascii_lowercase),
        "draft": v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        "checks": checks_state,
        "failing": failing,
        "review": review,
    })
}

// ---- worktree names ---------------------------------------------------------------------

/// A worktree name from a thread's first message: its first words, plus a few random
/// characters so two threads never pick the same one.
pub fn worktree_name(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(5)
        .map(str::to_ascii_lowercase)
        .collect();
    let mut name = words.join("-");
    name.truncate(32);
    let name = name.trim_end_matches('-');
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..4];
    if name.is_empty() {
        format!("littleguys-{tag}")
    } else {
        format!("{name}-{tag}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> CoderPrefs {
        CoderPrefs { on: true, ..CoderPrefs::default() }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lg-code-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // Temp dirs on macOS sit behind a symlink; git reports the real path. Windows writes
        // the real path as `\\?\C:\…`, which git can't use.
        let real = std::fs::canonicalize(&dir).unwrap();
        match real.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            Some(plain) => PathBuf::from(plain),
            None => real,
        }
    }

    fn run(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            // Your own git config (signing, hooks) stays out of it.
            .env("GIT_CONFIG_GLOBAL", std::env::temp_dir().join("lg-code-no-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Pat")
            .env("GIT_AUTHOR_EMAIL", "pat@example.com")
            .env("GIT_COMMITTER_NAME", "Pat")
            .env("GIT_COMMITTER_EMAIL", "pat@example.com")
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// `shop`: a repo on `main` with one commit, and a remote that has it.
    fn shop() -> (PathBuf, PathBuf) {
        let dir = temp_dir("shop");
        let remote = dir.join("remote.git");
        let shop = dir.join("shop");
        std::fs::create_dir_all(&shop).unwrap();
        run(&dir, &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
        run(&shop, &["init", "-q", "-b", "main"]);
        write(&shop.join("src/cart.rs"), "fn total() -> f64 {\n    1.0\n}\n");
        run(&shop, &["add", "."]);
        run(&shop, &["commit", "-q", "-m", "Start the shop"]);
        run(&shop, &["remote", "add", "origin", remote.to_str().unwrap()]);
        run(&shop, &["push", "-q", "-u", "origin", "main"]);
        run(&shop, &["remote", "set-head", "origin", "main"]);
        (dir, shop)
    }

    fn clear_caches() {
        STATUS.lock().unwrap().clear();
        BASES.lock().unwrap().clear();
    }

    #[test]
    fn git_paths_read_as_this_systems_own() {
        // (Paths compare equal whichever slash they use, so this compares what's shown.)
        let shown = |printed: &str| native_path(printed).display().to_string();
        if cfg!(windows) {
            assert_eq!(shown("C:/Users/pat/shop/.git"), r"C:\Users\pat\shop\.git");
        } else {
            assert_eq!(shown("/Users/pat/shop/.git"), "/Users/pat/shop/.git");
        }
    }

    #[test]
    fn porcelain_reads_branch_changes_and_conflicts() {
        let out = "# branch.oid abc123\0# branch.head fix-rounding\0# branch.upstream origin/fix-rounding\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa bbb src/cart.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 src/new name.rs\0src/old.rs\0\
u UU N... 100644 100644 100644 100644 aaa bbb ccc README.md\0\
? notes/\0? todo.txt\0";
        let p = parse_porcelain(out);
        assert_eq!(p.head.as_deref(), Some("abc123"));
        assert_eq!(p.branch.as_deref(), Some("fix-rounding"));
        assert_eq!(p.upstream.as_deref(), Some("origin/fix-rounding"));
        assert_eq!((p.ahead, p.behind), (2, 1));
        assert_eq!(p.changed, vec!["src/cart.rs", "src/new name.rs", "README.md"]);
        assert_eq!(p.untracked, vec!["notes/", "todo.txt"]);
        assert_eq!(p.conflicted, 1);

        let fresh = parse_porcelain("# branch.oid (initial)\0# branch.head main\0");
        assert_eq!(fresh.head, None);
        let detached = parse_porcelain("# branch.oid abc\0# branch.head (detached)\0");
        assert_eq!(detached.branch, None);
    }

    #[test]
    fn numstat_reads_renames_and_binaries() {
        let stats = parse_numstat("3\t1\tsrc/cart.rs\0-\t-\tlogo.png\x005\t0\t\0old.rs\0new.rs\0");
        assert_eq!(
            stats,
            vec![
                NumStat { path: "src/cart.rs".into(), old_path: None, added: Some(3), removed: Some(1) },
                NumStat { path: "logo.png".into(), old_path: None, added: None, removed: None },
                NumStat { path: "new.rs".into(), old_path: Some("old.rs".into()), added: Some(5), removed: Some(0) },
            ]
        );
        let names = parse_name_status("M\0src/cart.rs\0A\0tests/rounding.rs\0R100\0old.rs\0new.rs\0D\0gone.rs\0");
        assert_eq!(names[0], ("modified", "src/cart.rs".to_string(), None));
        assert_eq!(names[1].0, "added");
        assert_eq!(names[2], ("renamed", "new.rs".to_string(), Some("old.rs".to_string())));
        assert_eq!(names[3].0, "deleted");
    }

    #[test]
    fn changes_count_from_the_base_or_the_upstream() {
        assert_eq!(compare_ref(Some("fix-rounding"), None, Some("origin/main")), "origin/main");
        assert_eq!(compare_ref(Some("main"), Some("origin/main"), Some("origin/main")), "origin/main");
        assert_eq!(compare_ref(Some("main"), None, Some("main")), "HEAD");
        assert_eq!(compare_ref(Some("main"), None, None), "HEAD");
        assert_eq!(compare_ref(None, None, Some("origin/main")), "origin/main");
    }

    #[tokio::test]
    async fn off_and_outside_a_repo_answer_without_git() {
        let dir = temp_dir("plain");
        assert_eq!(status(&dir, &CoderPrefs::default()).await.unwrap(), json!({ "repo": null, "why": "off" }));
        assert_eq!(status(&dir, &on()).await.unwrap(), json!({ "repo": null, "why": "not-a-repo" }));
        assert_eq!(pr(&dir, &on()).await.unwrap(), json!({ "unavailable": "off" }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_feature_branch_counts_its_commits_and_what_isnt_committed() {
        let (dir, shop) = shop();
        clear_caches();
        let clean = status(&shop.join("src"), &on()).await.unwrap();
        assert_eq!(clean["repo"]["name"], "shop");
        assert_eq!(clean["branch"], "main");
        assert_eq!(clean["changes"]["files"], 0);
        assert_eq!(clean["dirty"], 0);

        run(&shop, &["checkout", "-q", "-b", "fix-rounding"]);
        write(&shop.join("src/cart.rs"), "fn total() -> f64 {\n    (1.0 * 100.0).round() / 100.0\n}\n");
        run(&shop, &["commit", "-q", "-am", "Round the total"]);
        write(&shop.join("tests/rounding.rs"), "#[test]\nfn rounds() {}\n");
        write(&shop.join("README.md"), "Shop\n");
        clear_caches();
        let st = status(&shop, &on()).await.unwrap();
        assert_eq!(st["branch"], "fix-rounding");
        assert_eq!(st["base"]["ref"], "origin/main");
        assert_eq!(st["commits"], 1);
        assert_eq!(st["dirty"], 2);
        assert_eq!(st["changes"]["files"], 3);
        assert_eq!(st["changes"]["untracked"], 2);
        assert_eq!(st["changes"]["added"], 1 + 2 + 1);
        assert_eq!(st["changes"]["removed"], 1);
        assert_eq!(st["repo"]["worktree"], false);

        let d = diff(&shop, None, &on()).await.unwrap();
        let paths: Vec<&str> = d["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert_eq!(paths, vec!["src/cart.rs", "README.md", "tests/rounding.rs"]);
        assert_eq!(d["files"][0]["status"], "modified");
        assert_eq!(d["files"][1]["status"], "untracked");
        let patch = d["patch"].as_str().unwrap();
        assert!(patch.contains("+    (1.0 * 100.0).round() / 100.0"), "{patch}");
        assert!(patch.contains("+++ b/tests/rounding.rs\n@@ -0,0 +1,2 @@\n+#[test]"), "{patch}");

        let one = diff(&shop, Some("src/cart.rs"), &on()).await.unwrap();
        assert_eq!(one["files"].as_array().unwrap().len(), 1);
        assert!(!one["patch"].as_str().unwrap().contains("rounding.rs"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn on_main_changes_are_what_isnt_pushed() {
        let (dir, shop) = shop();
        write(&shop.join("src/cart.rs"), "fn total() -> f64 {\n    2.0\n}\n");
        run(&shop, &["commit", "-q", "-am", "Double it"]);
        clear_caches();
        let st = status(&shop, &on()).await.unwrap();
        assert_eq!(st["branch"], "main");
        assert_eq!(st["upstream"], "origin/main");
        assert_eq!(st["ahead"], 1);
        assert_eq!(st["commits"], 1);
        assert_eq!(st["dirty"], 0);
        assert_eq!(st["changes"]["files"], 1);
        assert_eq!((st["changes"]["added"].as_u64(), st["changes"]["removed"].as_u64()), (Some(1), Some(1)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_worktree_names_its_repo_and_a_fresh_repo_works_too() {
        let (dir, shop) = shop();
        let wt = dir.join("wt-coupons");
        run(&shop, &["worktree", "add", "-q", "-b", "coupons", wt.to_str().unwrap()]);
        write(&wt.join("src/coupon.rs"), "pub struct Coupon;\n");
        clear_caches();
        let st = status(&wt, &on()).await.unwrap();
        assert_eq!(st["repo"]["worktree"], true);
        assert_eq!(st["repo"]["name"], "shop");
        assert_eq!(st["repo"]["mainRoot"], shop.display().to_string());
        assert_eq!(st["branch"], "coupons");
        assert_eq!(st["changes"]["files"], 1);

        let blog = dir.join("blog");
        std::fs::create_dir_all(&blog).unwrap();
        run(&blog, &["init", "-q", "-b", "main"]);
        write(&blog.join("post.md"), "# Hello\n\nFirst post.\n");
        let st = status(&blog, &on()).await.unwrap();
        assert_eq!(st["head"], Value::Null);
        assert_eq!(st["detached"], false);
        assert_eq!(st["changes"]["files"], 1);
        assert_eq!(st["changes"]["added"], 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pull_requests_read_their_checks() {
        let v = json!({
            "number": 31, "url": "https://github.com/pat/shop/pull/31", "title": "Coupons", "state": "OPEN", "isDraft": false,
            "reviewDecision": "REVIEW_REQUIRED",
            "statusCheckRollup": [
                { "__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "FAILURE" },
                { "__typename": "CheckRun", "name": "lint", "status": "IN_PROGRESS", "conclusion": "" },
                { "__typename": "StatusContext", "context": "deploy", "state": "SUCCESS" },
            ],
        });
        let pr = pr_summary(&v);
        assert_eq!(pr["checks"], "failing");
        assert_eq!(pr["failing"], json!(["test"]));
        assert_eq!(pr["review"], "pending");
        assert_eq!(pr["state"], "open");

        let running = pr_summary(&json!({ "statusCheckRollup": [{ "name": "lint", "status": "QUEUED", "conclusion": "" }] }));
        assert_eq!(running["checks"], "running");
        assert_eq!(pr_summary(&json!({ "statusCheckRollup": [] }))["checks"], "none");
        let passing = pr_summary(&json!({ "statusCheckRollup": [{ "name": "t", "status": "COMPLETED", "conclusion": "SUCCESS" }] }));
        assert_eq!(passing["checks"], "passing");

        assert_eq!(pr_error("no pull requests found for branch \"main\""), json!({ "pr": null }));
        assert_eq!(pr_error("To get started with GitHub CLI, please run:  gh auth login"), json!({ "unavailable": "logged-out" }));
        assert_eq!(pr_error("none of the git remotes configured for this repository point to a known GitHub host"), json!({ "unavailable": "not-github" }));
    }

    #[test]
    fn worktree_names_come_from_the_message() {
        let name = worktree_name("Fix the cart total's rounding, please!");
        assert!(name.starts_with("fix-the-cart-total-s-"), "{name}");
        assert_eq!(name.len(), "fix-the-cart-total-s-".len() + 4);
        assert!(worktree_name("¿¡").starts_with("littleguys-"));
        assert!(worktree_name("a very long first word supercalifragilisticexpialidocious").len() <= 37);
    }

    #[test]
    fn prefs_default_and_read_partially() {
        let p: CoderPrefs = serde_json::from_value(json!({ "on": true })).unwrap();
        assert!(p.on && p.panel && !p.worktrees && !p.prs);
        // A prefs file from before the terminal button: the button is on, in the system terminal.
        assert!(p.terminal && p.terminal_app.is_none());
        assert_eq!(serde_json::to_value(CoderPrefs::default()).unwrap()["panel"], true);
    }
}
