//! Path resolution for `thurmd` inside its macOS nested helper bundle
//! (`Thurm.app/Contents/Helpers/Thurm Daemon.app/Contents/MacOS/thurmd`) as well as flat
//! layouts (`target/debug/thurmd`, `~/.local/share/thurm/bin/thurmd`, Linux, and legacy
//! `Thurm.app/Contents/Helpers/thurmd`).

use std::path::{Path, PathBuf};

/// If `exe` is the executable of a nested `.app` bundle (`<dir>/<Name>.app/Contents/MacOS/<bin>`),
/// returns `<dir>` (e.g. `Thurm.app/Contents/Helpers`).
pub fn daemon_bundle_parent(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.parent().map(Path::to_path_buf))?
}

/// Directory holding sibling helper binaries (`thurm`, `thurm-intelligence`, `thurm-tailnet`):
/// the parent of `Thurm Daemon.app` (`Thurm.app/Contents/Helpers`) when running from a nested
/// helper bundle, else `exe.parent()`.
pub fn helpers_dir(exe: &Path) -> Option<PathBuf> {
    daemon_bundle_parent(exe).or_else(|| exe.parent().map(Path::to_path_buf))
}

/// Finds a sibling helper file `name` next to `exe`, or in `daemon_bundle_parent(exe)` when `exe`
/// lives inside `Thurm Daemon.app/Contents/MacOS`.
pub fn find_helper(exe: &Path, name: &str) -> Option<PathBuf> {
    let direct = exe.parent()?.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    let outer = daemon_bundle_parent(exe)?.join(name);
    outer.is_file().then_some(outer)
}

/// Expected path of sibling helper `name`: [`find_helper`] if present on disk, otherwise
/// `helpers_dir(exe)/<name>` (for clear error messages when missing).
pub fn helper_path(exe: &Path, name: &str) -> Option<PathBuf> {
    find_helper(exe, name).or_else(|| helpers_dir(exe).map(|d| d.join(name)))
}

/// `Contents/` of the enclosing macOS app bundle when `exe` sits in `Contents/Helpers/thurmd` or
/// `Contents/Helpers/Thurm Daemon.app/Contents/MacOS/thurmd` (or, for non-bundle layouts, the
/// parent of `helpers_dir`).
pub fn app_contents(exe: &Path) -> Option<PathBuf> {
    helpers_dir(exe)?.parent().map(Path::to_path_buf)
}

/// The main app executable (`Thurm.app/Contents/MacOS/Thurm`) when `exe` runs inside a macOS app
/// bundle (`Thurm.app/Contents/Helpers/Thurm Daemon.app/Contents/MacOS/thurmd` or legacy
/// `Thurm.app/Contents/Helpers/thurmd`); `None` outside a bundle.
#[cfg(any(target_os = "macos", test))]
pub fn bundle_app(exe: &Path) -> Option<PathBuf> {
    let helpers = helpers_dir(exe)?;
    let contents = helpers.parent()?;
    let bundle = contents.parent()?;
    (helpers.file_name()? == "Helpers"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| contents.join("MacOS").join("Thurm"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_daemon_bundle_paths() {
        let exe = Path::new(
            "/Applications/Thurm.app/Contents/Helpers/Thurm Daemon.app/Contents/MacOS/thurmd",
        );
        assert_eq!(
            daemon_bundle_parent(exe).as_deref(),
            Some(Path::new("/Applications/Thurm.app/Contents/Helpers"))
        );
        assert_eq!(
            helpers_dir(exe).as_deref(),
            Some(Path::new("/Applications/Thurm.app/Contents/Helpers"))
        );
        assert_eq!(
            app_contents(exe).as_deref(),
            Some(Path::new("/Applications/Thurm.app/Contents"))
        );
        assert_eq!(
            bundle_app(exe).as_deref(),
            Some(Path::new("/Applications/Thurm.app/Contents/MacOS/Thurm"))
        );
    }

    #[test]
    fn legacy_flat_app_bundle_paths() {
        let exe = Path::new("/Applications/Thurm.app/Contents/Helpers/thurmd");
        assert_eq!(daemon_bundle_parent(exe), None);
        assert_eq!(
            helpers_dir(exe).as_deref(),
            Some(Path::new("/Applications/Thurm.app/Contents/Helpers"))
        );
        assert_eq!(
            app_contents(exe).as_deref(),
            Some(Path::new("/Applications/Thurm.app/Contents"))
        );
        assert_eq!(
            bundle_app(exe).as_deref(),
            Some(Path::new("/Applications/Thurm.app/Contents/MacOS/Thurm"))
        );
    }

    #[test]
    fn non_bundle_and_standalone_paths() {
        let dev = Path::new("/Users/me/thurm/target/debug/thurmd");
        assert_eq!(daemon_bundle_parent(dev), None);
        assert_eq!(
            helpers_dir(dev).as_deref(),
            Some(Path::new("/Users/me/thurm/target/debug"))
        );
        assert_eq!(bundle_app(dev), None);

        // Standalone daemon bundle outside Thurm.app/Contents/Helpers must not match a parent app.
        let standalone = Path::new("/tmp/Thurm Daemon.app/Contents/MacOS/thurmd");
        assert_eq!(
            daemon_bundle_parent(standalone).as_deref(),
            Some(Path::new("/tmp"))
        );
        assert_eq!(bundle_app(standalone), None);
    }

    #[test]
    fn find_helper_in_nested_and_flat_layouts() {
        let base = std::env::temp_dir().join(format!("thurm-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let helpers = base.join("Thurm.app/Contents/Helpers");
        let daemon_macos = helpers.join("Thurm Daemon.app/Contents/MacOS");
        std::fs::create_dir_all(&daemon_macos).unwrap();
        let exe = daemon_macos.join("thurmd");
        std::fs::write(&exe, b"").unwrap();
        std::fs::write(helpers.join("thurm"), b"").unwrap();
        std::fs::write(helpers.join("thurm-tailnet"), b"").unwrap();
        std::fs::write(helpers.join("thurm-intelligence"), b"").unwrap();

        assert_eq!(find_helper(&exe, "thurm"), Some(helpers.join("thurm")));
        assert_eq!(
            find_helper(&exe, "thurm-tailnet"),
            Some(helpers.join("thurm-tailnet"))
        );
        assert_eq!(
            helper_path(&exe, "thurm-intelligence"),
            Some(helpers.join("thurm-intelligence"))
        );
        assert_eq!(find_helper(&exe, "missing"), None);
        assert_eq!(helper_path(&exe, "missing"), Some(helpers.join("missing")));

        let flat_exe = helpers.join("thurmd");
        assert_eq!(find_helper(&flat_exe, "thurm"), Some(helpers.join("thurm")));

        let _ = std::fs::remove_dir_all(&base);
    }
}
