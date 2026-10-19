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

/// What the overlay contributes to a load.
pub struct Merged {
    /// The text to load.
    pub text: String,
    /// What was left out of the overlay, and why.
    pub warnings: Vec<String>,
    /// The overlay's settings that took effect, as dotted paths (a table's contents by path).
    pub applied: Vec<String>,
}

/// `base` (the text of `config.toml`) with `overlay` merged over it. The overlay cannot fail a
/// load: a setting that names no option, does not parse or does not validate is left out with a
/// warning. A `base` that is not a valid config is an error.
pub fn merge(base: &str, overlay: &str) -> Result<Merged, String> {
    let base_table: Table = toml::from_str(base).map_err(|e| e.to_string())?;
    let base_warnings = Config::parse_with_warnings(base)?.1;
    let mut warnings = Vec::new();
    let mut overlay_table: Table = match toml::from_str(overlay) {
        Ok(table) => table,
        Err(e) => {
            return Ok(Merged {
                text: base.to_owned(),
                warnings: vec![format!("settings.toml ignored: {e}")],
                applied: vec![],
            });
        }
    };
    if overlay_table.remove("config_version").is_some() {
        warnings.push("settings.toml: config_version is not a setting, ignored".to_owned());
    }
    let settings = leaves(&overlay_table);
    if settings.is_empty() {
        return Ok(Merged {
            text: base.to_owned(),
            warnings,
            applied: vec![],
        });
    }
    let mut merged = base_table.clone();
    merge_tables(&mut merged, &overlay_table);
    let whole = toml::to_string(&merged).map_err(|e| e.to_string())?;
    if names_options(&overlay_table)
        && let Ok((_, new)) = Config::parse_with_warnings(&whole)
        && new.iter().all(|w| base_warnings.contains(w))
    {
        let applied = settings.iter().map(|(path, _)| path.join(".")).collect();
        return Ok(Merged {
            text: whole,
            warnings,
            applied,
        });
    }
    // Something in the overlay is wrong: keep every setting that works, one at a time.
    let mut current = base_table;
    let mut applied = Vec::new();
    for (path, value) in settings {
        let key = path.join(".");
        let mut alone = Table::new();
        set_path(&mut alone, &path, value.clone());
        let mut candidate = current.clone();
        set_path(&mut candidate, &path, value);
        let text = toml::to_string(&candidate).map_err(|e| e.to_string())?;
        match Config::parse_with_warnings(&text) {
            Ok((_, new))
                if names_options(&alone) && new.iter().all(|w| base_warnings.contains(w)) =>
            {
                current = candidate;
                applied.push(key);
            }
            Ok(_) => warnings.push(format!("settings.toml: {key} is not a setting, ignored")),
            Err(e) => warnings.push(format!("settings.toml: {key} ignored: {e}")),
        }
    }
    let text = toml::to_string(&current).map_err(|e| e.to_string())?;
    Ok(Merged {
        text,
        warnings,
        applied,
    })
}

/// Whether every setting in `table` names an option. Checked on its own: comparing with the
/// warnings of `config.toml` would let an unknown key the file also sets slip through. Values
/// are judged in the merged config, so one that fails to load here on its own still passes.
fn names_options(table: &Table) -> bool {
    let Ok(text) = toml::to_string(table) else {
        return false;
    };
    Config::parse_with_warnings(&text).map_or(true, |(_, warnings)| warnings.is_empty())
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
    let warnings = merge(&base, &out)?.warnings;
    let prefix = format!("settings.toml: {key} ");
    if let Some(w) = warnings.iter().find(|w| w.starts_with(&prefix)) {
        return Err(w.clone());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    replace_file(&file, out.as_bytes()).map_err(|e| format!("{}: {e}", file.display()))
}

/// Applies `edit`, a rewrite of the `[[remote]]` entries, to the overlay beside `config`. An
/// overlay that doesn't set `remote` yet starts from config.toml's hosts, so none go missing;
/// one whose last host is removed keeps `remote = []` while config.toml has hosts, or they
/// would come back. Returns the overlay's path.
pub fn edit_remotes(
    config: &Path,
    edit: impl FnOnce(&str) -> Result<String, String>,
) -> Result<PathBuf, String> {
    let file = path_beside(config);
    let base = read(config).map_err(|e| format!("{}: {e}", config.display()))?;
    let base_hosts = base
        .parse::<DocumentMut>()
        .map_err(|e| format!("{}: {e}", config.display()))?
        .remove("remote")
        .and_then(host_tables);
    let text = read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    // Comments with no settings under them would end up below the hosts.
    let mut lead = String::new();
    if doc.is_empty() {
        lead = if text.trim().is_empty() {
            HEADER
        } else {
            text.trim_end()
        }
        .to_owned();
        doc = DocumentMut::new();
    }
    // The `with_remote_*` edits take `[[remote]]` tables; `remote = []` (no hosts) has none.
    let hosts = match doc.remove("remote") {
        None => base_hosts.clone(),
        Some(hosts) => host_tables(hosts),
    };
    if let Some(hosts) = hosts {
        doc.insert("remote", hosts);
    }
    let mut doc: DocumentMut = edit(&doc.to_string())?
        .parse()
        .map_err(|e| format!("{e}"))?;
    if base_hosts.is_some() && !doc.contains_key("remote") {
        doc.insert("remote", toml_edit::value(toml_edit::Array::new()));
    }
    let out = match lead.trim_end() {
        "" => doc.to_string(),
        lead => format!("{lead}\n\n{doc}"),
    };
    let warnings = merge(&base, &out)?.warnings;
    if let Some(w) = warnings
        .iter()
        .find(|w| w.starts_with("settings.toml: remote "))
    {
        return Err(w.clone());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    replace_file(&file, out.as_bytes()).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(file)
}

/// `remote` as `[[remote]]` tables, also when written inline (`remote = [{ name = … }]`); `None`
/// when it holds no hosts.
fn host_tables(hosts: Item) -> Option<Item> {
    match hosts.into_array_of_tables() {
        Ok(tables) if tables.is_empty() => None,
        Ok(tables) => Some(Item::ArrayOfTables(tables)),
        Err(other) if other.as_array().is_some_and(|a| a.is_empty()) => None,
        Err(other) => Some(other),
    }
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
        [last] => match table.get_mut(last) {
            // An empty table names a setting without a value: it must not clear what is there.
            Some(Value::Table(existing)) if matches!(&value, Value::Table(_)) => {
                if let Value::Table(new) = &value {
                    merge_tables(existing, new);
                }
            }
            _ => {
                table.insert(last.clone(), value);
            }
        },
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

/// The settings of `table` with their paths: its values, arrays of tables, and empty tables (a
/// table with contents is only its contents).
fn leaves(table: &Table) -> Vec<(Vec<String>, Value)> {
    fn walk(table: &Table, prefix: &mut Vec<String>, out: &mut Vec<(Vec<String>, Value)>) {
        for (key, value) in table {
            prefix.push(key.clone());
            match value {
                Value::Table(child) if !child.is_empty() => walk(child, prefix, out),
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

    fn merged_hosts(config: &Path) -> Vec<(String, String)> {
        let overlay = read(&path_beside(config)).unwrap();
        let text = merge(&std::fs::read_to_string(config).unwrap(), &overlay)
            .unwrap()
            .text;
        Config::parse(&text)
            .unwrap()
            .remote
            .into_iter()
            .map(|r| (r.name, r.host))
            .collect()
    }

    fn remote(name: &str, host: &str) -> crate::RemoteConfig {
        crate::RemoteConfig {
            name: name.into(),
            host: host.into(),
            socket: None,
            enabled: true,
            clipboard_read: crate::ClipboardRead::Ask,
            tailnet: None,
        }
    }

    fn pair(name: &str, host: &str) -> (String, String) {
        (name.into(), host.into())
    }

    #[test]
    fn hosts_edited_in_the_overlay_start_from_the_configs() {
        let config = temp_config(
            "remotes-seed",
            "[[remote]]\nname = \"pi\"\nhost = \"me@pi\"\n",
        );
        let add = remote("devbox", "me@devbox");
        edit_remotes(&config, |t| crate::with_remote_added(t, &add)).unwrap();
        assert_eq!(
            merged_hosts(&config),
            [pair("pi", "me@pi"), pair("devbox", "me@devbox")]
        );
        let overlay = read(&path_beside(&config)).unwrap();
        assert!(overlay.starts_with(HEADER), "{overlay}");

        edit_remotes(&config, |t| crate::with_remote_removed(t, "pi")).unwrap();
        edit_remotes(&config, |t| crate::with_remote_removed(t, "devbox")).unwrap();
        assert_eq!(merged_hosts(&config), []);
        assert!(holds(&read(&path_beside(&config)).unwrap(), "remote"));

        edit_remotes(&config, |t| crate::with_remote_added(t, &add)).unwrap();
        assert_eq!(merged_hosts(&config), [pair("devbox", "me@devbox")]);
    }

    #[test]
    fn hosts_edited_in_the_overlay_without_config_hosts() {
        let config = temp_config("remotes-none", "[font]\nsize = 14.0\n");
        let add = remote("devbox", "me@devbox");
        edit_remotes(&config, |t| crate::with_remote_added(t, &add)).unwrap();
        assert_eq!(merged_hosts(&config), [pair("devbox", "me@devbox")]);
        edit_remotes(&config, |t| {
            crate::with_remote_setting(t, "devbox", "enabled", "false")
        })
        .unwrap();
        let overlay = read(&path_beside(&config)).unwrap();
        assert!(overlay.contains("enabled = false"), "{overlay}");
        edit_remotes(&config, |t| crate::with_remote_removed(t, "devbox")).unwrap();
        assert!(!holds(&read(&path_beside(&config)).unwrap(), "remote"));
        assert_eq!(merged_hosts(&config), []);
    }

    #[test]
    fn inline_overlay_hosts_can_be_edited() {
        let config = temp_config("remotes-inline-overlay", "[font]\nsize = 14.0\n");
        std::fs::write(
            path_beside(&config),
            "remote = [{ name = \"pi\", host = \"me@pi\", clipboard_read = \"never\" }]\n",
        )
        .unwrap();
        let add = remote("devbox", "me@devbox");
        edit_remotes(&config, |t| crate::with_remote_added(t, &add)).unwrap();
        edit_remotes(&config, |t| {
            crate::with_remote_setting(t, "devbox", "enabled", "false")
        })
        .unwrap();
        edit_remotes(&config, |t| crate::with_remote_removed(t, "pi")).unwrap();
        assert_eq!(merged_hosts(&config), [pair("devbox", "me@devbox")]);
        let overlay = read(&path_beside(&config)).unwrap();
        assert!(overlay.contains("enabled = false"), "{overlay}");

        std::fs::write(
            path_beside(&config),
            "remote = [{ name = \"pi\", host = \"me@pi\", clipboard_read = \"never\" }]\n",
        )
        .unwrap();
        edit_remotes(&config, |t| crate::with_remote_added(t, &add)).unwrap();
        let overlay = read(&path_beside(&config)).unwrap();
        assert!(overlay.contains("clipboard_read = \"never\""), "{overlay}");
    }

    #[test]
    fn inline_config_hosts_are_seeded_as_tables() {
        let config = temp_config(
            "remotes-inline",
            "remote = [{ name = \"pi\", host = \"me@pi\" }]\n",
        );
        let add = remote("devbox", "me@devbox");
        edit_remotes(&config, |t| crate::with_remote_added(t, &add)).unwrap();
        assert_eq!(
            merged_hosts(&config),
            [pair("pi", "me@pi"), pair("devbox", "me@devbox")]
        );
    }

    #[test]
    fn overlay_wins_key_by_key() {
        let base = "[font]\nsize = 14.0\nfamily = \"Menlo\"\n";
        let overlay = "[font]\nsize = 15.0\n[window]\ntab_style = \"native\"\n";
        let Merged { text, warnings, .. } = merge(base, overlay).unwrap();
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
        let Merged { text, warnings, .. } = merge(base, overlay).unwrap();
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
        let Merged { text, warnings, .. } =
            merge("[font]\nsize = 14.0\n", "[font\nsize =").unwrap();
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
    fn config_version_in_the_overlay_never_applies() {
        let fast = merge(
            DEFAULT_CONFIG_TOML,
            "config_version = 3\n[font]\nsize = 15.0\n",
        )
        .unwrap();
        let slow = merge(
            DEFAULT_CONFIG_TOML,
            "config_version = 3\n[font]\nsize = 1.0\n",
        )
        .unwrap();
        for merged in [fast, slow] {
            assert!(
                merged.warnings.iter().any(|w| w.contains("config_version")),
                "{:?}",
                merged.warnings
            );
            assert_eq!(Config::parse(&merged.text).unwrap().config_version, 9);
        }
    }

    #[test]
    fn an_unknown_key_config_toml_also_sets_is_still_reported() {
        let base = "[window]\nnonsense = 1\n";
        let merged = merge(base, "[window]\nnonsense = 2\n").unwrap();
        assert!(
            merged
                .warnings
                .iter()
                .any(|w| w.starts_with("settings.toml: window.nonsense ")),
            "{:?}",
            merged.warnings
        );
        assert!(merged.applied.is_empty(), "{:?}", merged.applied);
        let merged = merge(base, "[window]\nnonsense = 2\n[font]\nsize = 15.0\n").unwrap();
        assert_eq!(merged.applied, vec!["font.size".to_owned()]);
    }

    #[test]
    fn unknown_empty_tables_are_reported() {
        let merged = merge("", "[nonsense]\n").unwrap();
        assert!(
            merged
                .warnings
                .iter()
                .any(|w| w.starts_with("settings.toml: nonsense ")),
            "{:?}",
            merged.warnings
        );
        let merged = merge("", "[keybindings]\n").unwrap();
        assert!(merged.warnings.is_empty(), "{:?}", merged.warnings);
        assert_eq!(merged.applied, vec!["keybindings".to_owned()]);
    }

    #[test]
    fn applied_lists_only_the_settings_that_took_effect() {
        let merged = merge("", "[font]\nsize = 1.0\nfamily = \"Menlo\"\n").unwrap();
        assert_eq!(merged.applied, vec!["font.family".to_owned()]);
        assert_eq!(merged.warnings.len(), 1, "{:?}", merged.warnings);
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
