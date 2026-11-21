//! Coder mode's terminal button: opens the terminal you use, in the folder a coding thread
//! works in. See docs/CODER-MODE.md, "Opening a terminal".
//!
//! Two commands (commands.rs): `coder_terminals` lists the terminals on this computer for
//! Settings → Coder mode, and `open_terminal` opens the chosen one for a thread. Neither is a
//! runtime call, so the phone bridge never carries them and a paired phone can't open windows
//! here. The folder always comes from the thread (`Runtime::local_folder`, the one `code.*`
//! reads), never from the webview, and every terminal starts from an argv, never a shell
//! line, so a folder named `it's "new"` is just a folder.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::code::CoderPrefs;
use crate::platform::COMPUTER;

/// A terminal app littleguys can open.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Terminal {
    /// What `terminalApp` saves: the `.app` folder on a Mac, `wt`, `pwsh`, `powershell` or
    /// `cmd` on a PC.
    pub app: String,
    pub name: String,
    /// The system's own, opened when none is picked.
    pub system: bool,
    /// It says it opens folders handed to it, so it starts in the thread's folder. An app
    /// that doesn't say so (and isn't one whose flags littleguys knows) may start in your
    /// home folder instead; Settings says so under the picker.
    pub folders: bool,
    #[serde(skip)]
    kind: Kind,
    /// The `.app` folder, or the program.
    #[serde(skip)]
    path: PathBuf,
}

/// How a terminal is told where to start.
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)] // Each computer makes only its own; the tests make them all.
enum Kind {
    /// `open -a <app> <folder>`: the app opens the folder it's handed.
    OpensFolders,
    Ghostty,
    WezTerm,
    Kitty,
    Alacritty,
    /// `wt.exe -d <folder>`.
    WindowsTerminal,
    /// PowerShell or Command Prompt, started in the folder, in a console of its own.
    Console,
}

/// Mac terminals littleguys knows, by bundle id. Ones that list folders in their `Info.plist`
/// are opened with `open -a` whatever their kind here.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const KNOWN: &[(&str, Kind)] = &[
    ("com.apple.Terminal", Kind::OpensFolders),
    ("com.googlecode.iterm2", Kind::OpensFolders),
    ("com.mitchellh.ghostty", Kind::Ghostty),
    ("dev.warp.Warp-Stable", Kind::OpensFolders),
    ("com.github.wez.wezterm", Kind::WezTerm),
    ("net.kovidgoyal.kitty", Kind::Kitty),
    ("org.alacritty", Kind::Alacritty),
    ("co.zeit.hyper", Kind::OpensFolders),
    ("org.tabby", Kind::OpensFolders),
    ("com.superlogical.rex", Kind::OpensFolders),
];

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SYSTEM_MAC: &str = "com.apple.Terminal";

/// What `terminalApp` may be on a PC.
const PC_TERMINALS: &[&str] = &["wt", "pwsh", "powershell", "cmd"];

// ---- what's installed -------------------------------------------------------------------

/// The terminals on this computer, the system's first, plus `saved` when it's somewhere else.
pub fn list(saved: Option<&str>) -> Vec<Terminal> {
    #[cfg(target_os = "macos")]
    {
        let mut dirs = vec![PathBuf::from("/Applications"), PathBuf::from("/Applications/Utilities")];
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join("Applications"));
        }
        dirs.push(PathBuf::from("/System/Applications/Utilities"));
        mac_terminals(&dirs, saved)
    }
    #[cfg(windows)]
    {
        let _ = saved;
        pc_terminals()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = saved;
        Vec::new()
    }
}

/// The terminal to open: the saved one while it's still here, else the system's. The second
/// value names the saved one when it's gone, so the UI can say so.
pub fn chosen(saved: Option<&str>) -> Result<(Terminal, Option<String>), String> {
    let system = || system_terminal().ok_or_else(|| format!("No terminal was found on this {COMPUTER}."));
    let Some(saved) = saved else { return Ok((system()?, None)) };
    #[cfg(target_os = "macos")]
    let found = mac_terminal(Path::new(saved));
    #[cfg(not(target_os = "macos"))]
    let found = list(None).into_iter().find(|t| t.app == saved);
    match found {
        Some(t) => Ok((t, None)),
        None => Ok((system()?, Some(display_name(saved)))),
    }
}

/// Terminal on a Mac (without reading every app's `Info.plist`); Windows Terminal or
/// Windows PowerShell on a PC.
fn system_terminal() -> Option<Terminal> {
    #[cfg(target_os = "macos")]
    return ["/System/Applications/Utilities/Terminal.app", "/Applications/Utilities/Terminal.app"]
        .iter()
        .find_map(|app| mac_terminal(Path::new(app)));
    #[cfg(not(target_os = "macos"))]
    return list(None).into_iter().find(|t| t.system);
}

fn display_name(app: &str) -> String {
    let path = Path::new(app);
    match path.extension().and_then(|e| e.to_str()) {
        Some("app") => path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| app.to_string()),
        _ => app.to_string(),
    }
}

/// `terminalApp` as Settings saves it: a `.app` on a Mac, one of [`PC_TERMINALS`] on a PC.
/// Whether it's installed isn't checked here: a terminal uninstalled later shouldn't keep the
/// other coder settings from saving.
pub fn check_app(app: Option<&str>) -> Result<(), String> {
    let Some(app) = app else { return Ok(()) };
    let ok = if cfg!(target_os = "macos") {
        let path = Path::new(app);
        path.is_absolute() && path.extension().is_some_and(|e| e == "app")
    } else if cfg!(windows) {
        PC_TERMINALS.contains(&app)
    } else {
        false
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{app} isn't a terminal littleguys can open."))
    }
}

/// What an app's `Info.plist` says: its bundle id, and whether it opens folders handed to it
/// (a document type of `public.folder`, `public.directory` or `public.item`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn app_info(app: &Path) -> Option<(Option<String>, bool)> {
    let info = plist::Value::from_file(app.join("Contents/Info.plist")).ok()?;
    let info = info.as_dictionary()?;
    let id = info.get("CFBundleIdentifier").and_then(plist::Value::as_string).map(str::to_string);
    let folders = info
        .get("CFBundleDocumentTypes")
        .and_then(plist::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_dictionary()?.get("LSItemContentTypes")?.as_array())
        .flatten()
        .any(|uti| matches!(uti.as_string(), Some("public.folder" | "public.directory" | "public.item")));
    Some((id, folders))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn known(id: &str) -> Option<Kind> {
    KNOWN.iter().find(|(known, _)| *known == id).map(|(_, kind)| *kind)
}

/// The app at `app` as a terminal, known or picked by hand.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_terminal(app: &Path) -> Option<Terminal> {
    let (id, folders) = app_info(app)?;
    let kind = match id.as_deref().and_then(known) {
        Some(_) if folders => Kind::OpensFolders,
        Some(kind) => kind,
        None => Kind::OpensFolders,
    };
    Some(Terminal {
        app: app.to_string_lossy().into_owned(),
        name: display_name(&app.to_string_lossy()),
        system: id.as_deref() == Some(SYSTEM_MAC),
        // Ones whose flags littleguys knows start in the folder either way.
        folders: folders || kind != Kind::OpensFolders,
        kind,
        path: app.to_path_buf(),
    })
}

/// The known terminals in `dirs` (each bundle id once, from the first dir that has it), the
/// system's first, then by name; then `saved` if it's an app they don't include.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_terminals(dirs: &[PathBuf], saved: Option<&str>) -> Vec<Terminal> {
    let mut found: Vec<(String, Terminal)> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "app") {
                continue;
            }
            let Some((Some(id), _)) = app_info(&path) else { continue };
            if known(&id).is_none() || found.iter().any(|(seen, _)| *seen == id) {
                continue;
            }
            if let Some(t) = mac_terminal(&path) {
                found.push((id, t));
            }
        }
    }
    let mut list: Vec<Terminal> = found.into_iter().map(|(_, t)| t).collect();
    list.sort_by(|a, b| b.system.cmp(&a.system).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    if let Some(saved) = saved.filter(|s| !list.iter().any(|t| t.app == *s)) {
        list.extend(mac_terminal(Path::new(saved)));
    }
    list
}

/// Windows Terminal when it's installed (the system's then), PowerShell 7, Windows PowerShell
/// and Command Prompt.
#[cfg(windows)]
fn pc_terminals() -> Vec<Terminal> {
    use crate::platform::find_program;
    // Windows Terminal is an app execution alias in WindowsApps, which isn't always on the
    // path littleguys reads and doesn't always read as a file.
    let wt = find_program("wt").or_else(|| {
        let alias = dirs::data_local_dir()?.join("Microsoft\\WindowsApps\\wt.exe");
        std::fs::symlink_metadata(&alias).is_ok().then_some(alias)
    });
    let system = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    let mut list = Vec::new();
    let mut add = |app: &str, name: &str, kind: Kind, path: PathBuf| {
        list.push(Terminal { app: app.into(), name: name.into(), system: false, folders: true, kind, path });
    };
    if let Some(wt) = wt {
        add("wt", "Windows Terminal", Kind::WindowsTerminal, wt);
    }
    if let Some(pwsh) = find_program("pwsh") {
        add("pwsh", "PowerShell 7", Kind::Console, pwsh);
    }
    let powershell = find_program("powershell").unwrap_or_else(|| system.join("System32\\WindowsPowerShell\\v1.0\\powershell.exe"));
    add("powershell", "Windows PowerShell", Kind::Console, powershell);
    let cmd = find_program("cmd").unwrap_or_else(|| system.join("System32\\cmd.exe"));
    add("cmd", "Command Prompt", Kind::Console, cmd);
    // Windows Terminal when it's there, else Windows PowerShell.
    let first = if list[0].kind == Kind::WindowsTerminal { 0 } else { list.iter().position(|t| t.app == "powershell").unwrap_or(0) };
    list[first].system = true;
    let system = list.remove(first);
    list.insert(0, system);
    list
}

// ---- starting one -----------------------------------------------------------------------

/// A terminal's start, as a program and its arguments.
#[derive(Debug, PartialEq)]
struct Launch {
    program: PathBuf,
    args: Vec<OsString>,
    /// Where it starts, for the consoles on a PC.
    dir: Option<PathBuf>,
    /// A console window of its own (`CREATE_NEW_CONSOLE`).
    new_console: bool,
}

fn launch(t: &Terminal, folder: &Path) -> Launch {
    let open = |args: Vec<OsString>| Launch { program: PathBuf::from("/usr/bin/open"), args, dir: None, new_console: false };
    let app = t.path.as_os_str().to_owned();
    let folder = folder.as_os_str().to_owned();
    let new = |rest: &[&OsString]| {
        let mut args: Vec<OsString> = vec!["-na".into(), app.clone(), "--args".into()];
        args.extend(rest.iter().map(|a| (*a).clone()));
        open(args)
    };
    match t.kind {
        Kind::OpensFolders => open(vec!["-a".into(), app.clone(), folder]),
        Kind::Ghostty => {
            let mut flag = OsString::from("--working-directory=");
            flag.push(&folder);
            new(&[&flag])
        }
        Kind::WezTerm => new(&[&"start".into(), &"--cwd".into(), &folder]),
        Kind::Kitty => new(&[&"--directory".into(), &folder]),
        Kind::Alacritty => new(&[&"--working-directory".into(), &folder]),
        Kind::WindowsTerminal => Launch { program: t.path.clone(), args: vec!["-d".into(), folder], dir: None, new_console: false },
        Kind::Console => Launch {
            program: t.path.clone(),
            args: if t.app == "cmd" { Vec::new() } else { vec!["-NoLogo".into()] },
            dir: Some(PathBuf::from(folder)),
            new_console: true,
        },
    }
}

/// Why a thread can't open a terminal right now, if it can't: coder mode or the switch is
/// off, its folder isn't on this computer, or it's gone.
fn refusal(prefs: &CoderPrefs, code: bool, folder: Option<&Path>) -> Option<String> {
    if !prefs.on {
        return Some("Coder mode is off.".into());
    }
    if !prefs.terminal {
        return Some("The terminal button is off in Settings → Coder mode.".into());
    }
    let local = format!("This thread's folder isn't on this {COMPUTER}.");
    if !code {
        return Some(local);
    }
    match folder {
        None => Some(local),
        Some(folder) if !folder.is_dir() => Some(format!("{} doesn't exist on this {COMPUTER}", folder.display())),
        Some(_) => None,
    }
}

/// Open the chosen terminal in the folder `session_key` works in. Answers `{opened, missing?}`:
/// the terminal that opened, and the saved one when it's gone and the system's opened instead.
pub async fn open_for_thread(runtime: &crate::runtime::Runtime, session_key: &str) -> Result<Value, String> {
    let prefs = crate::code::prefs();
    let code = runtime.status().capabilities.code;
    let folder = if code { runtime.local_folder(Some(session_key), None).await } else { None };
    if let Some(why) = refusal(&prefs, code, folder.as_deref()) {
        return Err(why);
    }
    let folder = folder.expect("checked above");
    let saved = prefs.terminal_app.clone();
    let (terminal, missing) = tokio::task::spawn_blocking(move || chosen(saved.as_deref())).await.map_err(|e| e.to_string())??;
    start(&terminal, &folder).await?;
    Ok(json!({ "opened": terminal.name, "missing": missing }))
}

async fn start(t: &Terminal, folder: &Path) -> Result<(), String> {
    let l = launch(t, folder);
    let mut cmd = tokio::process::Command::new(&l.program);
    cmd.args(&l.args);
    if let Some(dir) = &l.dir {
        cmd.current_dir(dir);
    }
    let failed = |e: std::io::Error| format!("{} didn't open: {e}", t.name);
    if cfg!(target_os = "macos") {
        // `open` hands the folder to the app and exits; it fails when the app can't start.
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let out = tokio::time::timeout(Duration::from_secs(15), cmd.output())
            .await
            .map_err(|_| format!("{} took too long to open.", t.name))?
            .map_err(failed)?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr);
            return Err(format!("{} didn't open: {}", t.name, why.trim()));
        }
        return Ok(());
    }
    // A terminal's whole point is its window: not `platform::command`, which hides consoles.
    // And no redirected stdio, so a console's shell gets the console's own.
    #[cfg(windows)]
    if l.new_console {
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        cmd.creation_flags(CREATE_NEW_CONSOLE);
    }
    #[cfg(not(windows))]
    let _ = l.new_console;
    cmd.spawn().map(drop).map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("littleguys-terminal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake `<name>.app` in `dir` whose `Info.plist` has `id` and these document types.
    fn fake_app(dir: &Path, name: &str, id: &str, utis: &[&str]) -> PathBuf {
        let app = dir.join(format!("{name}.app"));
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        let types: String = utis
            .iter()
            .map(|u| format!("<dict><key>LSItemContentTypes</key><array><string>{u}</string></array></dict>"))
            .collect();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleDocumentTypes</key><array>{types}</array>
</dict></plist>"#
        );
        std::fs::write(app.join("Contents/Info.plist"), plist).unwrap();
        app
    }

    fn term(kind: Kind, app: &str, path: &str) -> Terminal {
        Terminal { app: app.into(), name: app.into(), system: false, folders: true, kind, path: PathBuf::from(path) }
    }

    fn args(l: &Launch) -> Vec<String> {
        l.args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn info_plists_say_whether_an_app_opens_folders() {
        let dir = temp_dir("plist");
        let terminal = fake_app(&dir, "Terminal", "com.apple.Terminal", &["com.apple.terminal.session", "public.directory"]);
        let rex = fake_app(&dir, "Rex", "com.superlogical.rex", &["public.item"]);
        let plainterm = fake_app(&dir, "Plainterm", "dev.example.plainterm", &[]);
        assert_eq!(app_info(&terminal), Some((Some("com.apple.Terminal".into()), true)));
        assert_eq!(app_info(&rex), Some((Some("com.superlogical.rex".into()), true)));
        assert_eq!(app_info(&plainterm), Some((Some("dev.example.plainterm".into()), false)));
        assert_eq!(app_info(&dir.join("Nothing.app")), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn known_terminals_are_listed_the_systems_first() {
        let apps = temp_dir("apps");
        let utilities = temp_dir("utilities");
        fake_app(&apps, "iTerm", "com.googlecode.iterm2", &["public.folder"]);
        fake_app(&apps, "Ghostty", "com.mitchellh.ghostty", &[]);
        fake_app(&apps, "Notes", "com.apple.Notes", &["public.folder"]);
        fake_app(&utilities, "Terminal", "com.apple.Terminal", &["public.directory"]);
        // The same app in a second folder is listed once.
        fake_app(&utilities, "iTerm", "com.googlecode.iterm2", &["public.folder"]);
        let plainterm = fake_app(&utilities, "Plainterm", "dev.example.plainterm", &[]);

        let list = mac_terminals(&[apps.clone(), utilities.clone()], None);
        let names: Vec<&str> = list.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Terminal", "Ghostty", "iTerm"]);
        assert!(list[0].system && !list[1].system);
        assert_eq!(list[2].app, apps.join("iTerm.app").to_string_lossy());
        // Ghostty doesn't list folders, but its flag starts it in one.
        assert_eq!((list[1].kind, list[1].folders), (Kind::Ghostty, true));

        // An app picked by hand is added, and flagged when it doesn't open folders.
        let saved = plainterm.to_string_lossy().into_owned();
        let list = mac_terminals(&[apps.clone(), utilities.clone()], Some(&saved));
        let last = list.last().unwrap();
        assert_eq!((last.name.as_str(), last.folders, last.kind), ("Plainterm", false, Kind::OpensFolders));
        let _ = std::fs::remove_dir_all(apps);
        let _ = std::fs::remove_dir_all(utilities);
    }

    #[test]
    fn each_terminal_starts_in_the_folder_from_an_argv() {
        let folder = Path::new("/Users/me/dev/it's \"new\" shop");
        let f = folder.to_string_lossy().into_owned();

        let l = launch(&term(Kind::OpensFolders, "/Applications/iTerm.app", "/Applications/iTerm.app"), folder);
        assert_eq!(l.program, PathBuf::from("/usr/bin/open"));
        assert_eq!(args(&l), ["-a", "/Applications/iTerm.app", f.as_str()]);

        let l = launch(&term(Kind::Ghostty, "g", "/Applications/Ghostty.app"), folder);
        assert_eq!(args(&l), ["-na".into(), "/Applications/Ghostty.app".into(), "--args".into(), format!("--working-directory={f}")]);

        let l = launch(&term(Kind::WezTerm, "w", "/Applications/WezTerm.app"), folder);
        assert_eq!(args(&l), ["-na", "/Applications/WezTerm.app", "--args", "start", "--cwd", f.as_str()]);

        let l = launch(&term(Kind::Kitty, "k", "/Applications/kitty.app"), folder);
        assert_eq!(args(&l), ["-na", "/Applications/kitty.app", "--args", "--directory", f.as_str()]);

        let l = launch(&term(Kind::Alacritty, "a", "/Applications/Alacritty.app"), folder);
        assert_eq!(args(&l), ["-na", "/Applications/Alacritty.app", "--args", "--working-directory", f.as_str()]);
        assert!(!l.new_console && l.dir.is_none());
    }

    #[test]
    fn pc_terminals_start_in_the_folder_too() {
        let folder = Path::new("C:\\Users\\me\\dev\\shop & co");
        let f = folder.to_string_lossy().into_owned();

        let l = launch(&term(Kind::WindowsTerminal, "wt", "C:\\wt.exe"), folder);
        assert_eq!((l.program.clone(), args(&l)), (PathBuf::from("C:\\wt.exe"), vec!["-d".to_string(), f.clone()]));
        assert!(!l.new_console);

        let l = launch(&term(Kind::Console, "pwsh", "C:\\pwsh.exe"), folder);
        assert_eq!((args(&l), l.dir.clone(), l.new_console), (vec!["-NoLogo".to_string()], Some(folder.to_path_buf()), true));

        let l = launch(&term(Kind::Console, "cmd", "C:\\cmd.exe"), folder);
        assert!(l.args.is_empty() && l.new_console && l.dir.as_deref() == Some(folder));
    }

    #[test]
    fn a_thread_opens_a_terminal_only_when_everything_allows_it() {
        let here = temp_dir("refusal");
        let on = CoderPrefs { on: true, ..CoderPrefs::default() };
        assert_eq!(refusal(&on, true, Some(&here)), None);
        assert_eq!(refusal(&CoderPrefs::default(), true, Some(&here)).as_deref(), Some("Coder mode is off."));
        let switch_off = CoderPrefs { terminal: false, ..on.clone() };
        assert!(refusal(&switch_off, true, Some(&here)).unwrap().contains("terminal button is off"));
        assert!(refusal(&on, false, Some(&here)).unwrap().contains("isn't on this"));
        assert!(refusal(&on, true, None).unwrap().contains("isn't on this"));
        let gone = here.join("archived-worktree");
        assert!(refusal(&on, true, Some(&gone)).unwrap().ends_with(&format!("doesn't exist on this {COMPUTER}")));
        let _ = std::fs::remove_dir_all(here);
    }

    #[test]
    fn only_a_terminal_can_be_saved() {
        assert!(check_app(None).is_ok());
        assert!(check_app(Some("rm -rf ~")).is_err());
        assert!(check_app(Some("/bin/zsh")).is_err());
        #[cfg(target_os = "macos")]
        {
            assert!(check_app(Some("/Applications/iTerm.app")).is_ok());
            assert!(check_app(Some("iTerm.app")).is_err());
        }
        #[cfg(windows)]
        {
            assert!(check_app(Some("wt")).is_ok() && check_app(Some("cmd")).is_ok());
            assert!(check_app(Some("C:\\evil.exe")).is_err());
        }
    }

    #[test]
    fn a_saved_terminal_thats_gone_falls_back_to_the_systems() {
        #[cfg(target_os = "macos")]
        {
            let (t, missing) = chosen(Some("/Applications/Long Gone Term.app")).unwrap();
            assert!(t.system);
            assert_eq!(missing.as_deref(), Some("Long Gone Term"));
        }
        #[cfg(windows)]
        {
            let (t, missing) = chosen(Some("nonesuch")).unwrap();
            assert!(t.system);
            assert_eq!(missing.as_deref(), Some("nonesuch"));
        }
    }
}
