//! The settings window's overlay: `settings.toml` beside `config.toml`.
//!
//! `config.toml` may be a read-only link (Home Manager points it into the Nix store), so the
//! settings window writes here instead. Tables merge key by key and the overlay wins per key; a
//! key the overlay leaves out comes from `config.toml`, then from the default. Migrations only
//! ever rewrite `config.toml`.

use std::io;
use std::path::{Path, PathBuf};

use toml::{Table, Value};
use toml_edit::{DocumentMut, Item, TableLike};

use crate::{Config, replace_file};

const FILE: &str = "settings.toml";

const HEADER: &str = "# Changed in Thurm's settings window. These win over config.toml, setting by\n\
# setting. Remove a line (or use Reset in the window) to go back to config.toml.\n";

/// The overlay beside the config file `config`.
pub fn path_beside(config: &Path) -> PathBuf {
    config.with_file_name(FILE)
}

/// The overlay's text; a missing file is empty.
pub fn read(overlay: &Path) -> io::Result<String> {
    match std::fs::read_to_string(overlay) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// Whether the overlay text sets `key` (a dotted path such as `font.size`).
pub fn holds(overlay: &str, key: &str) -> bool {
    toml::from_str::<Table>(overlay)
        .ok()
        .is_some_and(|t| get_path(&t, &segments(key)).is_some())
}

/// `base` (the text of `config.toml`) with `overlay` merged over it, as the text to load and the
/// warnings about the overlay. The overlay cannot fail a load: a setting that names no option,
/// does not parse or does not validate is left out with a warning. A `base` that is not a valid
/// config is an error.
pub fn merge(base: &str, overlay: &str) -> Result<(String, Vec<String>), String> {
    let base_table: Table = toml::from_str(base).map_err(|e| e.to_string())?;
    let base_warnings = Config::parse_with_warnings(base)?.1;
    let overlay_table: Table = match toml::from_str(overlay) {
        Ok(table) => table,
        Err(e) => return Ok((base.to_owned(), vec![format!("settings.toml ignored: {e}")])),
    };
    if overlay_table.is_empty() {
        return Ok((base.to_owned(), vec![]));
    }
    let mut merged = base_table.clone();
    merge_tables(&mut merged, &overlay_table);
    let whole = toml::to_string(&merged).map_err(|e| e.to_string())?;
    if let Ok((_, warnings)) = Config::parse_with_warnings(&whole)
        && warnings.iter().all(|w| base_warnings.contains(w))
    {
        return Ok((whole, vec![]));
    }
    // Something in the overlay is wrong: keep every setting that works, one at a time.
    let mut current = base_table;
    let mut warnings = Vec::new();
    for (path, value) in leaves(&overlay_table) {
        let key = path.join(".");
        if path == ["config_version"] {
            warnings.push(format!("settings.toml: {key} is not a setting, ignored"));
            continue;
        }
        let mut candidate = current.clone();
        set_path(&mut candidate, &path, value);
        let text = toml::to_string(&candidate).map_err(|e| e.to_string())?;
        match Config::parse_with_warnings(&text) {
            Ok((_, new)) if new.iter().all(|w| base_warnings.contains(w)) => current = candidate,
            Ok(_) => warnings.push(format!("settings.toml: {key} is not a setting, ignored")),
            Err(e) => warnings.push(format!("settings.toml: {key} ignored: {e}")),
        }
    }
    let text = toml::to_string(&current).map_err(|e| e.to_string())?;
    Ok((text, warnings))
}

/// Sets `key` in the overlay beside `config` to the TOML value `literal`, or removes it for
/// `None`, keeping the overlay's comments and formatting. Refused, and nothing is written, when
/// the result would not load the setting.
pub fn write(config: &Path, key: &str, literal: Option<&str>) -> Result<(), String> {
    let file = path_beside(config);
    let text = read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let value = literal
        .map(|l| {
            l.parse::<toml_edit::Value>()
                .map_err(|e| format!("{l:?}: {e}"))
        })
        .transpose()?;
    let out = edit(&text, key, value)?;
    let base = read(config).map_err(|e| format!("{}: {e}", config.display()))?;
    let (_, warnings) = merge(&base, &out)?;
    let prefix = format!("settings.toml: {key} ");
    if let Some(w) = warnings.iter().find(|w| w.starts_with(&prefix)) {
        return Err(w.clone());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    replace_file(&file, out.as_bytes()).map_err(|e| format!("{}: {e}", file.display()))
}

/// The overlay beside `config`, created with a short header when it does not exist yet.
pub fn ensure(config: &Path) -> io::Result<PathBuf> {
    let file = path_beside(config);
    if !file.exists() {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        replace_file(&file, HEADER.as_bytes())?;
    }
    Ok(file)
}

fn edit(text: &str, key: &str, value: Option<toml_edit::Value>) -> Result<String, String> {
    let path: Vec<&str> = key.split('.').collect();
    if path.iter().any(|p| p.is_empty()) {
        return Err(format!("expected a setting name, got {key:?}"));
    }
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    match value {
        Some(value) => {
            let (last, parents) = path.split_last().expect("a key has a name");
            let mut table: &mut dyn TableLike = doc.as_table_mut();
            for name in parents {
                table = table
                    .entry(name)
                    .or_insert(toml_edit::table())
                    .as_table_like_mut()
                    .ok_or_else(|| format!("[{name}] is not a table"))?;
            }
            match table.get_mut(last).and_then(Item::as_value_mut) {
                // Replace just the value: the key keeps its comments.
                Some(existing) => {
                    let decor = existing.decor().clone();
                    *existing = value;
                    *existing.decor_mut() = decor;
                }
                None => {
                    table.insert(last, Item::Value(value));
                }
            }
        }
        None => {
            remove_path(doc.as_table_mut(), &path);
        }
    }
    Ok(doc.to_string())
}

/// Removes the key at `path`, and the tables it leaves empty. Whether it was there.
fn remove_path(table: &mut dyn TableLike, path: &[&str]) -> bool {
    match path {
        [] => false,
        [last] => table.remove(last).is_some(),
        [first, rest @ ..] => {
            let Some(child) = table.get_mut(first).and_then(Item::as_table_like_mut) else {
                return false;
            };
            let removed = remove_path(child, rest);
            if removed && child.is_empty() {
                table.remove(first);
            }
            removed
        }
    }
}

fn segments(key: &str) -> Vec<String> {
    key.split('.').map(str::to_owned).collect()
}

fn get_path<'a>(table: &'a Table, path: &[String]) -> Option<&'a Value> {
    match path {
        [] => None,
        [last] => table.get(last),
        [first, rest @ ..] => match table.get(first)? {
            Value::Table(child) => get_path(child, rest),
            _ => None,
        },
    }
}

fn set_path(table: &mut Table, path: &[String], value: Value) {
    match path {
        [] => {}
        [last] => {
            table.insert(last.clone(), value);
        }
        [first, rest @ ..] => {
            let child = table
                .entry(first.clone())
                .or_insert_with(|| Value::Table(Table::new()));
            if !matches!(child, Value::Table(_)) {
                *child = Value::Table(Table::new());
            }
            if let Value::Table(child) = child {
                set_path(child, rest, value);
            }
        }
    }
}

fn merge_tables(base: &mut Table, overlay: &Table) {
    for (key, value) in overlay {
        match (base.get_mut(key), value) {
            (Some(Value::Table(b)), Value::Table(o)) => merge_tables(b, o),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// The non-table values of `table` with their paths (arrays of tables count as values).
fn leaves(table: &Table) -> Vec<(Vec<String>, Value)> {
    fn walk(table: &Table, prefix: &mut Vec<String>, out: &mut Vec<(Vec<String>, Value)>) {
        for (key, value) in table {
            prefix.push(key.clone());
            match value {
                Value::Table(child) => walk(child, prefix, out),
                _ => out.push((prefix.clone(), value.clone())),
            }
            prefix.pop();
        }
    }
    let mut out = Vec::new();
    walk(table, &mut Vec::new(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_CONFIG_TOML, TabStyle, migrate_config};

    fn temp_config(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("thurm-overlay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, text).unwrap();
        config
    }

    #[test]
    fn overlay_wins_key_by_key() {
        let base = "[font]\nsize = 14.0\nfamily = \"Menlo\"\n";
        let overlay = "[font]\nsize = 15.0\n[window]\ntab_style = \"native\"\n";
        let (text, warnings) = merge(base, overlay).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let cfg = Config::parse(&text).unwrap();
        assert_eq!((cfg.font.size, cfg.font.family.as_str()), (15.0, "Menlo"));
        assert_eq!(cfg.window.tab_style, TabStyle::Native);
        assert!(cfg.window.pane_headers);
    }

    #[test]
    fn unknown_and_invalid_settings_warn_and_are_left_out() {
        let base = "[font]\nsize = 14.0\n";
        let overlay =
            "[font]\nfamliy = \"x\"\nsize = 1.0\n[window]\nopacity = \"half\"\nblur = 3\n";
        let (text, warnings) = merge(base, overlay).unwrap();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("settings.toml: font.famliy "))
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("settings.toml: font.size ignored"))
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("settings.toml: window.opacity ignored"))
        );
        let cfg = Config::parse(&text).unwrap();
        assert_eq!(
            (cfg.font.size, cfg.window.opacity, cfg.window.blur),
            (14.0, 1.0, 3)
        );
    }

    #[test]
    fn an_unreadable_overlay_is_ignored_with_a_warning() {
        let (text, warnings) = merge("[font]\nsize = 14.0\n", "[font\nsize =").unwrap();
        assert_eq!(text, "[font]\nsize = 14.0\n");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn a_broken_config_file_is_still_an_error() {
        assert!(merge("[font\n", "[window]\nblur = 3\n").is_err());
    }

    #[test]
    fn reset_removes_the_key_and_the_tables_it_leaves_empty() {
        let config = temp_config("reset", DEFAULT_CONFIG_TOML);
        write(&config, "font.size", Some("15.0")).unwrap();
        write(&config, "notifications.waiting.notify", Some("false")).unwrap();
        let file = path_beside(&config);
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("size = 15.0")
        );
        write(&config, "font.size", None).unwrap();
        write(&config, "notifications.waiting.notify", None).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains("font") && !text.contains("notifications"),
            "{text}"
        );
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn writes_keep_the_overlays_comments_and_refuse_what_does_not_load() {
        let config = temp_config("comments", "[font]\nsize = 14.0\n");
        let file = path_beside(&config);
        std::fs::write(&file, "# mine\n[font]\n# the size\nsize = 15.0\n").unwrap();
        write(&config, "font.family", Some("\"Menlo\"")).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("# mine") && text.contains("# the size"),
            "{text}"
        );
        assert!(text.contains("family = \"Menlo\""), "{text}");
        assert!(write(&config, "font.size", Some("1.0")).is_err());
        assert!(write(&config, "font.famliy", Some("\"x\"")).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_read_only_config_stays_as_it_is_and_migrations_ignore_the_overlay() {
        use std::os::unix::fs::PermissionsExt;
        let old = "[font]\nsize = 14.0\n";
        let config = temp_config("readonly", old);
        let file = path_beside(&config);
        std::fs::write(&file, "[window]\npane_headers = false\n").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o444)).unwrap();

        write(&config, "font.family", Some("\"Menlo\"")).unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), old);
        let overlay_before = std::fs::read_to_string(&file).unwrap();
        migrate_config(&config);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), old);

        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        migrate_config(&config);
        let migrated = std::fs::read_to_string(&config).unwrap();
        assert!(migrated.contains("config_version"), "{migrated}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), overlay_before);
        let (cfg, warnings) = crate::Config::load_with_warnings_from(&config).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!cfg.window.pane_headers);
        assert_eq!(cfg.font.family, "Menlo");
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn ensure_creates_an_empty_overlay_once() {
        let config = temp_config("ensure", DEFAULT_CONFIG_TOML);
        let file = ensure(&config).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with('#') && !holds(&text, "font.size"));
        std::fs::write(&file, "[font]\nsize = 15.0\n").unwrap();
        ensure(&config).unwrap();
        assert!(holds(&std::fs::read_to_string(&file).unwrap(), "font.size"));
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }
}
