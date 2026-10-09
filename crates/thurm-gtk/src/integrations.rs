//! Thurm › Integrations (Integrations.swift) and the app's small host services: the bundled
//! fonts and icons, the state file, dropped images, and the file manager.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use adw::prelude::*;
use gtk::{gio, glib};
use serde_json::Value;

use crate::app::{self, App};
use crate::dialogs;

// MARK: fonts

#[link(name = "fontconfig")]
unsafe extern "C" {
    fn FcConfigAppFontAddDir(config: *mut std::ffi::c_void, dir: *const u8) -> i32;
}

/// The directory with the app's shared files (`share/thurm` next to `bin/`, or the repo when
/// run from a build tree).
pub fn share_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("THURM_SHARE").map(PathBuf::from) {
        return Some(d);
    }
    let exe = std::env::current_exe().ok()?;
    let bin = exe.parent()?;
    let candidates = [
        bin.join("../share/thurm"),
        dirs_data().join("thurm"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../linux/share"),
    ];
    candidates.into_iter().find(|d| d.join("fonts").is_dir() || d.join("skills").is_dir())
}

fn dirs_data() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Makes the bundled JetBrains Mono and Symbols Nerd Font Mono available to this process.
pub fn register_fonts() {
    let mut dirs: Vec<PathBuf> = share_dir().map(|d| d.join("fonts")).into_iter().collect();
    // A build tree: the macOS bundle's fonts.
    dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../macos/Resources/fonts"));
    if let Some(d) = dirs.into_iter().find(|d| d.is_dir()) {
        let path = format!("{}\0", d.display());
        unsafe { FcConfigAppFontAddDir(std::ptr::null_mut(), path.as_ptr()) };
    }
}

/// The symbolic icons the app uses, for icon themes without them (GTK falls back to hicolor).
pub fn register_icons() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let mut dirs: Vec<PathBuf> = share_dir().map(|d| d.join("icons")).into_iter().collect();
    dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../linux/icons"));
    if let Some(d) = dirs.into_iter().find(|d| d.is_dir()) {
        gtk::IconTheme::for_display(&display).add_search_path(d);
    }
}

// MARK: state

fn state_file() -> PathBuf {
    thurm_config::state_dir().join("gtk-state.json")
}

fn load_state() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(state_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn load_state_list(key: &str) -> Vec<String> {
    load_state()
        .get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Written to a temporary file and renamed into place, so a crash never leaves half a file
/// (which would read as empty).
pub fn save_state_list(key: &str, list: &[String]) -> std::io::Result<()> {
    let mut state = load_state();
    state.insert(key.into(), Value::from(list.to_vec()));
    let path = state_file();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let written = std::fs::write(&tmp, Value::Object(state).to_string())
        .and_then(|()| std::fs::rename(&tmp, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

// MARK: files

/// Writes a pasted or dropped image to a directory only this user can read
/// (`$XDG_RUNTIME_DIR/thurm-drops`, else in Thurm's state directory), removing ones older than
/// a day.
pub fn save_drop_image(png: &[u8]) -> Option<String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute() && d.is_dir())
        .unwrap_or_else(thurm_config::state_dir);
    let dir = base.join("thurm-drops");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).ok()?;
    // Not a symlink to somewhere else, and private even if it existed before.
    if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
        return None;
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age > Duration::from_secs(24 * 3600));
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = dir.join(format!("image-{:08x}.png", (nanos as u64) ^ std::process::id() as u64));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    std::io::Write::write_all(&mut file, png).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// Shows a file in the file manager (never opens it).
pub fn show_in_file_manager(path: &str) {
    let uri = gio::File::for_path(path).uri();
    let shown = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
        .ok()
        .and_then(|bus| {
            bus.call_sync(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1",
                "ShowItems",
                Some(&(vec![uri.to_string()], String::new()).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                2000,
                None::<&gio::Cancellable>,
            )
            .ok()
        })
        .is_some();
    if !shown && let Some(parent) = Path::new(path).parent() {
        let _ = gio::AppInfo::launch_default_for_uri(
            &gio::File::for_path(parent).uri(),
            None::<&gio::AppLaunchContext>,
        );
    }
}

// MARK: integrations

fn bin_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

fn bundled_cli() -> Option<PathBuf> {
    bin_dir().map(|d| d.join("thurm")).filter(|p| p.is_file())
}

fn cli_link() -> PathBuf {
    home().join(".local/bin/thurm")
}

/// The CLI is reachable: `thurm` next to the app is what PATH finds (packages install it so).
fn cli_installed() -> bool {
    let Some(cli) = bundled_cli() else { return true };
    let link = cli_link();
    std::fs::canonicalize(&link).ok() == std::fs::canonicalize(&cli).ok()
        || thurm_config::which("thurm")
            .and_then(|p| std::fs::canonicalize(p).ok())
            == std::fs::canonicalize(&cli).ok()
}

fn skill_source() -> Option<PathBuf> {
    let mut c: Vec<PathBuf> = share_dir().map(|d| d.join("skills/thurm")).into_iter().collect();
    c.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills/thurm"));
    c.into_iter().find(|p| p.join("SKILL.md").is_file())
}

fn skill_link() -> PathBuf {
    home().join(".claude/skills/thurm")
}

fn systemd_enabled() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "thurmd.service"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `thurm --json hooks status`: one entry per agent with a config directory.
fn hooks_status() -> Vec<Value> {
    let Some(cli) = bundled_cli().or_else(|| thurm_config::which("thurm")) else {
        return Vec::new();
    };
    std::process::Command::new(cli)
        .args(["--json", "hooks", "status"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|h| h.get("present").and_then(Value::as_bool) == Some(true))
        .collect()
}

fn hooks_complete(h: &Value) -> bool {
    let installed = h.get("installed").and_then(Value::as_u64).unwrap_or(0);
    let total = h.get("total").and_then(Value::as_u64).unwrap_or(1);
    installed >= total
}

pub fn menu() -> gio::Menu {
    let menu = gio::Menu::new();
    if bundled_cli().is_some() {
        menu.append(
            Some(if cli_installed() { "✓ Uninstall Command-Line Tool" } else { "Install Command-Line Tool" }),
            Some("app.integ_cli"),
        );
    }
    for h in hooks_status() {
        let name = h.get("name").and_then(Value::as_str).unwrap_or("agent");
        let agent = h.get("agent").and_then(Value::as_str).unwrap_or("");
        let label = if hooks_complete(&h) {
            format!("✓ Uninstall {name} Hooks")
        } else {
            format!("Install {name} Hooks")
        };
        let item = gio::MenuItem::new(Some(&label), None);
        item.set_action_and_target_value(Some("app.integ_hooks"), Some(&agent.to_variant()));
        menu.append_item(&item);
    }
    if home().join(".claude").is_dir() && skill_source().is_some() {
        let linked = std::fs::canonicalize(skill_link()).ok() == skill_source().and_then(|s| std::fs::canonicalize(s).ok());
        menu.append(
            Some(if linked {
                "✓ Uninstall thurm Skill for Claude Code"
            } else {
                "Install thurm Skill for Claude Code"
            }),
            Some("app.integ_skill"),
        );
    }
    menu.append(
        Some(if systemd_enabled() { "✓ Don't Start Daemon at Login" } else { "Start Daemon at Login" }),
        Some("app.integ_systemd"),
    );
    menu
}

/// The integrations as palette entries (title, action).
pub fn palette_items() -> Vec<(String, Rc<dyn Fn()>)> {
    let mut out: Vec<(String, Rc<dyn Fn()>)> = Vec::new();
    if bundled_cli().is_some() {
        let label = if cli_installed() { "Uninstall Command-Line Tool" } else { "Install Command-Line Tool" };
        out.push((label.into(), Rc::new(|| app::with_app(|a| a.activate("integ_cli")).unwrap_or(()))));
    }
    let label = if systemd_enabled() { "Don't Start Daemon at Login" } else { "Start Daemon at Login" };
    out.push((label.into(), Rc::new(|| app::with_app(|a| a.activate("integ_systemd")).unwrap_or(()))));
    out
}

pub fn register_actions(app: &Rc<App>) {
    let cli = gio::SimpleAction::new("integ_cli", None);
    cli.connect_activate(|_, _| app::with_app(toggle_cli).unwrap_or(()));
    app.gtk_app.add_action(&cli);
    let hooks = gio::SimpleAction::new("integ_hooks", Some(glib::VariantTy::STRING));
    hooks.connect_activate(|_, p| {
        if let Some(agent) = p.and_then(|v| v.str().map(str::to_string)) {
            app::with_app(|a| toggle_hooks(a, &agent));
        }
    });
    app.gtk_app.add_action(&hooks);
    let skill = gio::SimpleAction::new("integ_skill", None);
    skill.connect_activate(|_, _| app::with_app(toggle_skill).unwrap_or(()));
    app.gtk_app.add_action(&skill);
    let systemd = gio::SimpleAction::new("integ_systemd", None);
    systemd.connect_activate(|_, _| app::with_app(toggle_systemd).unwrap_or(()));
    app.gtk_app.add_action(&systemd);
}

fn report(app: &App, title: &str, result: Result<String, String>) {
    match result {
        Ok(msg) => app.toast(&msg, 4.0),
        Err(e) => {
            if let Some(w) = app.win() {
                dialogs::inform(&w.window, title, &e);
            }
        }
    }
}

fn toggle_cli(app: &Rc<App>) {
    let Some(cli) = bundled_cli() else { return };
    let link = cli_link();
    let result = if cli_installed() && std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
        std::fs::remove_file(&link)
            .map(|_| format!("Removed {}.", link.display()))
            .map_err(|e| e.to_string())
    } else if std::fs::symlink_metadata(&link).is_ok_and(|m| !m.file_type().is_symlink()) {
        Err(format!("{} exists and is not a link; not replacing it.", link.display()))
    } else {
        let _ = std::fs::remove_file(&link);
        if let Some(d) = link.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        std::os::unix::fs::symlink(&cli, &link)
            .map(|_| format!("Linked {} → {}.", link.display(), cli.display()))
            .map_err(|e| e.to_string())
    };
    report(app, "Command-Line Tool", result);
}

fn toggle_hooks(app: &Rc<App>, agent: &str) {
    let Some(h) = hooks_status().into_iter().find(|h| h.get("agent").and_then(Value::as_str) == Some(agent)) else {
        return;
    };
    let name = h.get("name").and_then(Value::as_str).unwrap_or(agent).to_string();
    let path = h.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    let install = !hooks_complete(&h);
    let (title, body, ok) = if install {
        (
            format!("Install {name} hooks?"),
            format!("Adds Thurm's hooks to {path} (a backup of the current file is kept), so Thurm shows when {name} is working, waiting for you or done. Running sessions pick them up after a restart."),
            "Install",
        )
    } else {
        (
            format!("Remove {name} hooks?"),
            format!("Removes Thurm's hooks from {path}; your other hooks stay."),
            "Remove",
        )
    };
    let Some(w) = app.win() else { return };
    let agent = agent.to_string();
    dialogs::confirm(&w.window, &title, &body, ok, !install, move || {
        run_cli(
            vec!["hooks".into(), if install { "install" } else { "uninstall" }.into(), "--agent".into(), agent.clone()],
            "Agent Hooks",
        );
    });
}

fn toggle_skill(app: &Rc<App>) {
    let Some(src) = skill_source() else { return };
    let link = skill_link();
    let linked = std::fs::canonicalize(&link).ok() == std::fs::canonicalize(&src).ok();
    let result = if linked {
        std::fs::remove_file(&link)
            .map(|_| format!("Removed {}.", link.display()))
            .map_err(|e| e.to_string())
    } else if std::fs::symlink_metadata(&link).is_ok() {
        Err(format!("{} exists; not replacing it.", link.display()))
    } else {
        if let Some(d) = link.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        std::os::unix::fs::symlink(&src, &link)
            .map(|_| format!("Linked {}. New Claude Code sessions can use it.", link.display()))
            .map_err(|e| e.to_string())
    };
    report(app, "thurm Skill", result);
}

fn toggle_systemd(_app: &Rc<App>) {
    let action = if systemd_enabled() { "uninstall-systemd" } else { "install-systemd" };
    run_cli(vec!["daemon".into(), action.into()], "Daemon at Login");
}

/// Runs the bundled `thurm` off the main thread and reports its output.
fn run_cli(args: Vec<String>, title: &'static str) {
    let Some(cli) = bundled_cli().or_else(|| thurm_config::which("thurm")) else {
        app::with_app(|a| report(a, title, Err("thurm is not installed".into())));
        return;
    };
    let proc = gio::Subprocess::newv(
        &std::iter::once(cli.as_os_str().to_owned())
            .chain(args.iter().map(std::ffi::OsString::from))
            .collect::<Vec<_>>()
            .iter()
            .map(|s| s.as_os_str())
            .collect::<Vec<_>>(),
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE,
    );
    let proc = match proc {
        Ok(p) => p,
        Err(e) => {
            app::with_app(|a| report(a, title, Err(format!("could not run thurm: {e}"))));
            return;
        }
    };
    let waited = proc.clone();
    proc.communicate_utf8_async(None, None::<&gio::Cancellable>, move |res| {
        let result = match res {
            Err(e) => Err(format!("thurm failed: {e}")),
            Ok((out, _)) => {
                let text = out.map(|s| s.trim().to_string()).unwrap_or_default();
                if waited.is_successful() {
                    Ok(text)
                } else if text.is_empty() {
                    Err(format!("thurm exited with status {}", waited.exit_status()))
                } else {
                    Err(text)
                }
            }
        };
        app::with_app(|a| report(a, title, result));
    });
}
