//! What differs between macOS, Windows and Linux: where programs live and how
//! to start and stop them, private files, and this computer's name.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::RwLock;

/// What people call this kind of computer in a sentence ("on this Mac", "on this PC").
#[cfg(target_os = "macos")]
pub const COMPUTER: &str = "Mac";
#[cfg(windows)]
pub const COMPUTER: &str = "PC";
#[cfg(all(unix, not(target_os = "macos")))]
pub const COMPUTER: &str = "computer";

// ---- PATH ---------------------------------------------------------------------------------

#[cfg(unix)]
const FALLBACK_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"];

static PATH: RwLock<Option<String>> = RwLock::new(None);

/// The PATH programs littleguys starts get (openclaw, claude, agents), cached.
///
/// macOS and Linux: GUI apps get a minimal PATH, so it's the login shell's PATH
/// (where nvm, brew and the official installers put node and the CLIs), plus
/// the usual dirs. Windows: the user's and the system's PATH as they're saved
/// now (an installer may have changed them since littleguys started), plus
/// the dirs npm, Node and the Claude Code and OpenClaw installers use.
pub fn login_path() -> String {
    if let Some(path) = PATH.read().unwrap().as_ref() {
        return path.clone();
    }
    refresh_login_path()
}

/// Work [`login_path`] out again (after installing something that changed it).
pub fn refresh_login_path() -> String {
    let mut dirs: Vec<PathBuf> = Vec::new();
    #[cfg(unix)]
    {
        if let Some(p) = shell_path() {
            dirs.extend(std::env::split_paths(&p));
        }
        dirs.extend(FALLBACK_DIRS.iter().map(PathBuf::from));
    }
    #[cfg(windows)]
    {
        for p in saved_paths().into_iter().chain(std::env::var("PATH").ok()) {
            dirs.extend(std::env::split_paths(&p));
        }
        let env_dir = |var: &str, rest: &str| std::env::var_os(var).map(|v| PathBuf::from(v).join(rest));
        dirs.extend(
            [
                env_dir("APPDATA", "npm"),
                env_dir("ProgramFiles", "nodejs"),
                env_dir("LOCALAPPDATA", "OpenClaw\\deps\\portable-node"),
                env_dir("USERPROFILE", ".local\\bin"),
                env_dir("USERPROFILE", ".bun\\bin"),
                env_dir("LOCALAPPDATA", "Microsoft\\WinGet\\Links"),
            ]
            .into_iter()
            .flatten(),
        );
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| !d.as_os_str().is_empty() && seen.insert(d.clone()));
    let path = std::env::join_paths(dirs).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    *PATH.write().unwrap() = Some(path.clone());
    path
}

/// Windows: the user's and the system's PATH as saved in the registry, `%VARS%` expanded.
#[cfg(windows)]
fn saved_paths() -> Vec<String> {
    const KEYS: [&str; 2] = ["HKCU\\Environment", "HKLM\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment"];
    KEYS.iter()
        .filter_map(|key| {
            let out = std_command("reg").args(["query", key, "/v", "Path"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
            parse_reg_path(&String::from_utf8_lossy(&out.stdout))
        })
        .map(|p| expand_env_vars(&p))
        .collect()
}

/// The value in `reg query <key> /v Path` output: `    Path    REG_EXPAND_SZ    C:\a;%X%\b`.
#[cfg(any(windows, test))]
fn parse_reg_path(out: &str) -> Option<String> {
    out.lines().find_map(|line| {
        let line = line.trim_start();
        let rest = line.strip_prefix("Path").or_else(|| line.strip_prefix("PATH"))?;
        let rest = rest.trim_start();
        let value = rest.strip_prefix("REG_EXPAND_SZ").or_else(|| rest.strip_prefix("REG_SZ"))?;
        Some(value.trim().to_string()).filter(|v| !v.is_empty())
    })
}

/// `%NAME%` replaced by the environment variable's value (left as is when unset).
#[cfg(any(windows, test))]
fn expand_env_vars(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(unix)]
fn shell_path() -> Option<String> {
    let default = if cfg!(target_os = "macos") { "/bin/zsh" } else { "/bin/sh" };
    let shell = std::env::var("SHELL").unwrap_or_else(|_| default.into());
    let output = std::process::Command::new(&shell)
        .args(["-lic", "printf '__PATH__%s' \"$PATH\""])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let (_, path) = text.rsplit_once("__PATH__")?;
    Some(path.trim().to_string()).filter(|p| !p.is_empty())
}

/// The dirs on [`login_path`], in order.
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::split_paths(&login_path()).collect()
}

/// `name` in `dirs`, the first one that holds it as a program. On Windows
/// `claude` also finds `claude.exe`, `npx` finds `npx.cmd`, and so on.
pub fn find_in<I: IntoIterator<Item = PathBuf>>(dirs: I, name: &str) -> Option<PathBuf> {
    dirs.into_iter().flat_map(|dir| program_candidates(&dir.join(name))).find(|p| is_executable(p))
}

/// `name` on [`login_path`].
pub fn find_program(name: &str) -> Option<PathBuf> {
    find_in(path_dirs(), name)
}

/// `path` itself, and on Windows with each program extension added when it has none.
pub fn program_candidates(path: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let has_ext = path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|e| program_exts().iter().any(|x| x.eq_ignore_ascii_case(e)));
        if !has_ext {
            let mut out: Vec<PathBuf> = program_exts()
                .iter()
                .map(|ext| {
                    let mut p = path.as_os_str().to_owned();
                    p.push(".");
                    p.push(ext);
                    PathBuf::from(p)
                })
                .collect();
            out.push(path.to_path_buf());
            return out;
        }
    }
    vec![path.to_path_buf()]
}

/// Whether `command` names a file rather than something to look up on PATH.
pub fn is_path_like(command: &str) -> bool {
    command.contains('/') || (cfg!(windows) && (command.contains('\\') || command.contains(':')))
}

/// A file this user can run.
pub fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        path.extension()
            .and_then(OsStr::to_str)
            .is_some_and(|e| program_exts().iter().any(|x| x.eq_ignore_ascii_case(e)))
    }
}

/// PATHEXT, lowercase and without dots ("exe", "cmd", …).
#[cfg(windows)]
fn program_exts() -> &'static [String] {
    static EXTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    EXTS.get_or_init(|| {
        let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        raw.split(';')
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    })
}

// ---- starting and stopping programs ---------------------------------------------------------

/// Keeps console programs from flashing a console window (Windows).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `program`, ready to run in the background (no console window on Windows).
pub fn command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    #[allow(unused_mut)]
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// [`command`], blocking.
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Start `cmd` as the head of its own process tree, so [`stop_tree`] can end
/// everything it spawns (`npx` and the agent under it).
pub fn own_process_group(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// Ask the tree started by [`own_process_group`] to stop (`force`: make it).
/// On Windows there's no polite signal for background programs, so it's always forced.
pub fn stop_tree(pid: u32, force: bool) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let signal = if force { 9 } else { 15 };
        // SAFETY: kill(2) has no memory-safety preconditions; a negative pid
        // targets the process group made by own_process_group.
        unsafe {
            kill(-(pid as i32), signal);
        }
    }
    #[cfg(windows)]
    {
        let _ = force;
        let _ = std_command("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The Tailscale CLI, where its installers put it (or on PATH).
pub fn tailscale_cli() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let known: Vec<PathBuf> =
        ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/usr/local/bin/tailscale", "/opt/homebrew/bin/tailscale"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
    #[cfg(windows)]
    let known: Vec<PathBuf> = std::env::var_os("ProgramFiles")
        .map(|p| PathBuf::from(p).join("Tailscale").join("tailscale.exe"))
        .into_iter()
        .collect();
    #[cfg(all(unix, not(target_os = "macos")))]
    let known: Vec<PathBuf> = ["/usr/bin/tailscale", "/usr/local/bin/tailscale"].into_iter().map(PathBuf::from).collect();
    known.into_iter().find(|p| p.exists()).or_else(|| find_program("tailscale"))
}

/// Windows: whether the taskbar is light (Settings → Personalization → Colors →
/// "Choose your default Windows mode"). Missing means dark, the default.
#[cfg(windows)]
pub fn taskbar_is_light() -> bool {
    let key = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    std_command("reg")
        .args(["query", key, "/v", "SystemUsesLightTheme"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .and_then(|out| out.lines().find(|l| l.contains("SystemUsesLightTheme")).map(|l| l.trim().ends_with("0x1")))
        .unwrap_or(false)
}

// ---- files ----------------------------------------------------------------------------------

/// Open options for a file only this user can read (0600 on macOS and Linux;
/// on Windows, files under the user's profile already are).
pub fn private_file() -> std::fs::OpenOptions {
    #[allow(unused_mut)]
    let mut opts = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts
}

// ---- this computer --------------------------------------------------------------------------

/// This computer's name, as people see it (macOS: System Settings → General → Sharing).
pub fn computer_name() -> String {
    #[cfg(target_os = "macos")]
    let name = std_command("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").ok();
    #[cfg(all(unix, not(target_os = "macos")))]
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|s| s.trim().to_string());
    name.filter(|n| !n.is_empty()).unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            "Mac".into()
        } else {
            "PC".into()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_in_dirs() {
        let dir = std::env::temp_dir().join(format!("littleguys-platform-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "tool.cmd" } else { "tool" };
        let file = dir.join(name);
        std::fs::write(&file, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(find_in([dir.clone()], "tool"), None, "not executable yet");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(find_in([dir.clone()], "tool"), Some(file));
        assert_eq!(find_in([dir.clone()], "nope"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_reg_query_output() {
        let out = "\r\nHKEY_CURRENT_USER\\Environment\r\n    Path    REG_EXPAND_SZ    C:\\Tools;%LG_TEST_HOME%\\bin\r\n\r\n";
        assert_eq!(parse_reg_path(out).as_deref(), Some("C:\\Tools;%LG_TEST_HOME%\\bin"));
        assert_eq!(parse_reg_path("ERROR: The system was unable to find the specified registry key or value."), None);
        std::env::set_var("LG_TEST_HOME", "C:\\Users\\pat");
        assert_eq!(expand_env_vars("%LG_TEST_HOME%\\bin;%LG_UNSET_VAR%;50%"), "C:\\Users\\pat\\bin;%LG_UNSET_VAR%;50%");
    }

    #[test]
    fn path_like_commands() {
        assert!(is_path_like("~/bin/agent"));
        assert!(is_path_like("/usr/local/bin/agent"));
        assert!(!is_path_like("npx"));
        assert_eq!(is_path_like("C:\\Tools\\agent.exe"), cfg!(windows));
    }
}
