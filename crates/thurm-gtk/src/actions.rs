//! Every command of the app, as the macOS menus list them (MainMenu.swift): its action name,
//! label, menu and default Linux shortcut. The menus, the key bindings (with `[keybindings]`
//! overrides) and the command palette are all built from this table.

use std::collections::HashMap;

use gtk::gdk;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Menu {
    App,
    Shell,
    Edit,
    View,
    Window,
    Help,
}

impl Menu {
    pub fn title(self) -> &'static str {
        match self {
            Menu::App => "Thurm",
            Menu::Shell => "Shell",
            Menu::Edit => "Edit",
            Menu::View => "View",
            Menu::Window => "Window",
            Menu::Help => "Help",
        }
    }

    /// The palette lists the app menu last.
    pub const PALETTE_ORDER: [Menu; 6] =
        [Menu::Shell, Menu::Edit, Menu::View, Menu::Window, Menu::Help, Menu::App];
}

pub struct ActionDef {
    pub name: &'static str,
    pub label: &'static str,
    pub menu: Menu,
    /// GTK accelerator strings.
    pub accels: &'static [&'static str],
}

const fn a(
    name: &'static str,
    label: &'static str,
    menu: Menu,
    accels: &'static [&'static str],
) -> ActionDef {
    ActionDef {
        name,
        label,
        menu,
        accels,
    }
}

/// In menu order. Names are the `[keybindings]` action names.
pub const ACTIONS: &[ActionDef] = &[
    // Thurm
    a("about", "About Thurm", Menu::App, &[]),
    a("open_config", "Settings…", Menu::App, &["<Control>comma"]),
    a("reload_config", "Reload Configuration", Menu::App, &["<Control><Shift>r"]),
    a("remotes", "Remotes…", Menu::App, &[]),
    a("quit", "Quit Thurm", Menu::App, &["<Control><Shift>q"]),
    // Shell
    a("new_tab", "New Tab", Menu::Shell, &["<Control><Shift>t"]),
    a("new_workspace", "New Workspace", Menu::Shell, &["<Control><Shift>n"]),
    a("switch_workspace", "Switch Workspace…", Menu::Shell, &["<Control><Alt>o"]),
    a("switch_agent", "Switch to Agent…", Menu::Shell, &["<Control><Alt>a"]),
    a("rename_workspace", "Rename Workspace…", Menu::Shell, &[]),
    a("close_workspace", "Close Workspace…", Menu::Shell, &[]),
    a("split_right", "Split Right", Menu::Shell, &["<Control><Shift>d", "<Control><Shift>o"]),
    a("split_down", "Split Down", Menu::Shell, &["<Control><Shift>e"]),
    a("command_palette", "Command Palette…", Menu::Shell, &["<Control><Shift>p"]),
    a("processes", "Processes & Ports…", Menu::Shell, &[]),
    a("close_pane", "Close Pane", Menu::Shell, &["<Control><Shift>w"]),
    a("close_tab", "Close Tab", Menu::Shell, &["<Control><Shift><Alt>w"]),
    // Edit
    a("copy", "Copy", Menu::Edit, &["<Control><Shift>c"]),
    a("paste", "Paste", Menu::Edit, &["<Control><Shift>v"]),
    a("select_all", "Select All", Menu::Edit, &["<Control><Shift>a"]),
    a("clear_screen", "Clear Screen", Menu::Edit, &["<Control><Shift>k"]),
    a("clear_scrollback", "Clear Scrollback", Menu::Edit, &["<Control><Shift><Alt>k"]),
    a("explain", "Explain Last Command", Menu::Edit, &[]),
    a("find", "Find…", Menu::Edit, &["<Control><Shift>f"]),
    a("find_next", "Find Next", Menu::Edit, &["<Control><Shift>g"]),
    a("find_previous", "Find Previous", Menu::Edit, &["<Control><Shift>h"]),
    // View
    a(
        "increase_font_size",
        "Increase Font Size",
        Menu::View,
        &["<Control>equal", "<Control>plus", "<Control><Shift>plus"],
    ),
    a("decrease_font_size", "Decrease Font Size", Menu::View, &["<Control>minus"]),
    a("reset_font_size", "Reset Font Size", Menu::View, &["<Control>0"]),
    a("toggle_tab_style", "Tabs in Sidebar", Menu::View, &[]),
    a("toggle_sidebar", "Hide Sidebar", Menu::View, &["<Control><Shift>b"]),
    a("browse_themes", "Theme: Browse Themes…", Menu::View, &[]),
    a("follow_appearance", "Theme: Match System Appearance", Menu::View, &[]),
    a("zoom_split", "Zoom Split", Menu::View, &["<Control><Shift>Return"]),
    a("equalize_splits", "Equalize Splits", Menu::View, &["<Control><Alt>equal"]),
    a("focus_left", "Select Split Left", Menu::View, &["<Control><Alt>Left"]),
    a("focus_right", "Select Split Right", Menu::View, &["<Control><Alt>Right"]),
    a("focus_up", "Select Split Above", Menu::View, &["<Control><Alt>Up"]),
    a("focus_down", "Select Split Below", Menu::View, &["<Control><Alt>Down"]),
    a("resize_left", "Move Divider Left", Menu::View, &["<Control><Shift><Alt>Left"]),
    a("resize_right", "Move Divider Right", Menu::View, &["<Control><Shift><Alt>Right"]),
    a("resize_up", "Move Divider Up", Menu::View, &["<Control><Shift><Alt>Up"]),
    a("resize_down", "Move Divider Down", Menu::View, &["<Control><Shift><Alt>Down"]),
    a("toggle_fullscreen", "Toggle Full Screen", Menu::View, &["F11"]),
    // Window
    a("minimize", "Minimize", Menu::Window, &[]),
    a("maximize", "Zoom", Menu::Window, &[]),
    a("previous_tab", "Show Previous Tab", Menu::Window, &["<Control>Page_Up"]),
    a("next_tab", "Show Next Tab", Menu::Window, &["<Control>Page_Down"]),
    a("select_tab_1", "Select Tab 1", Menu::Window, &["<Alt>1"]),
    a("select_tab_2", "Select Tab 2", Menu::Window, &["<Alt>2"]),
    a("select_tab_3", "Select Tab 3", Menu::Window, &["<Alt>3"]),
    a("select_tab_4", "Select Tab 4", Menu::Window, &["<Alt>4"]),
    a("select_tab_5", "Select Tab 5", Menu::Window, &["<Alt>5"]),
    a("select_tab_6", "Select Tab 6", Menu::Window, &["<Alt>6"]),
    a("select_tab_7", "Select Tab 7", Menu::Window, &["<Alt>7"]),
    a("select_tab_8", "Select Tab 8", Menu::Window, &["<Alt>8"]),
    a("select_last_tab", "Select Last Tab", Menu::Window, &["<Alt>9"]),
    a("move_tab_to_new_workspace", "Move Tab to New Workspace", Menu::Window, &[]),
    a("quick_terminal", "Quick Terminal", Menu::Window, &[]),
    a("workspace_1", "Workspace 1", Menu::Window, &["<Control><Alt>1"]),
    a("workspace_2", "Workspace 2", Menu::Window, &["<Control><Alt>2"]),
    a("workspace_3", "Workspace 3", Menu::Window, &["<Control><Alt>3"]),
    a("workspace_4", "Workspace 4", Menu::Window, &["<Control><Alt>4"]),
    a("workspace_5", "Workspace 5", Menu::Window, &["<Control><Alt>5"]),
    a("workspace_6", "Workspace 6", Menu::Window, &["<Control><Alt>6"]),
    a("workspace_7", "Workspace 7", Menu::Window, &["<Control><Alt>7"]),
    a("workspace_8", "Workspace 8", Menu::Window, &["<Control><Alt>8"]),
    a("workspace_9", "Workspace 9", Menu::Window, &["<Control><Alt>9"]),
    // Help
    a("open_config_file", "Open Configuration File", Menu::Help, &[]),
];

/// Actions only reachable through key bindings (no menu item of their own).
pub fn hidden_from_menu(name: &str) -> bool {
    name.starts_with("workspace_")
}

pub fn find(name: &str) -> Option<&'static ActionDef> {
    ACTIONS.iter().find(|a| a.name == name)
}

const MODS: gdk::ModifierType = gdk::ModifierType::CONTROL_MASK
    .union(gdk::ModifierType::SHIFT_MASK)
    .union(gdk::ModifierType::ALT_MASK)
    .union(gdk::ModifierType::SUPER_MASK);

/// A key combination: lowercase keyval plus the modifiers that matter.
pub type Combo = (u32, gdk::ModifierType);

pub fn combo(keyval: gdk::Key, mods: gdk::ModifierType) -> Combo {
    (keyval.to_lower().into_glib(), mods & MODS)
}

/// `<Control><Shift>t` → (key, modifiers), without GTK (tests run without a display).
pub fn parse_accel(accel: &str) -> Option<(gdk::Key, gdk::ModifierType)> {
    let mut mods = gdk::ModifierType::empty();
    let mut rest = accel;
    while let Some(r) = rest.strip_prefix('<') {
        let (m, after) = r.split_once('>')?;
        mods |= match m {
            "Control" | "Primary" | "Ctrl" => gdk::ModifierType::CONTROL_MASK,
            "Shift" => gdk::ModifierType::SHIFT_MASK,
            "Alt" => gdk::ModifierType::ALT_MASK,
            "Super" => gdk::ModifierType::SUPER_MASK,
            _ => return None,
        };
        rest = after;
    }
    let key = gdk::Key::from_name(rest)?;
    Some((key, mods))
}

/// Parses a `[keybindings]` key: `ctrl+shift+d`, `alt+1`, `f11`, and the macOS form, where
/// `cmd` is Ctrl+Shift (and `cmd+shift`, Ctrl+Shift+Alt). Returns a GTK accelerator.
pub fn parse_binding(spec: &str) -> Option<String> {
    let parts: Vec<String> = spec
        .split('+')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    // "cmd++" ends in an empty part for the plus key.
    let (key, mods) = if spec.trim_end().ends_with("++") {
        ("plus".to_string(), &parts[..])
    } else {
        let (k, m) = parts.split_last()?;
        (k.clone(), m)
    };
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut sup = false;
    let mut cmd = false;
    for m in mods {
        match m.as_str() {
            "cmd" | "command" => cmd = true,
            "super" | "win" | "meta" | "logo" => sup = true,
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "alt" | "opt" | "option" => alt = true,
            _ => return None,
        }
    }
    if cmd {
        // macOS ⌘ maps to Ctrl+Shift; ⇧⌘ needs one more modifier to stay distinct.
        if shift {
            alt = true;
        }
        ctrl = true;
        shift = true;
    }
    let name = match key.as_str() {
        "plus" | "+" => "plus".to_string(),
        "minus" | "-" => "minus".into(),
        "equal" | "=" => "equal".into(),
        "comma" | "," => "comma".into(),
        "period" | "." => "period".into(),
        "slash" | "/" => "slash".into(),
        "backslash" | "\\" => "backslash".into(),
        "semicolon" | ";" => "semicolon".into(),
        "quote" | "'" => "apostrophe".into(),
        "grave" | "`" | "backtick" => "grave".into(),
        "left_bracket" | "[" => "bracketleft".into(),
        "right_bracket" | "]" => "bracketright".into(),
        "enter" | "return" => "Return".into(),
        "esc" | "escape" => "Escape".into(),
        "space" => "space".into(),
        "tab" => "Tab".into(),
        "backspace" => "BackSpace".into(),
        "delete" => "Delete".into(),
        "left" => "Left".into(),
        "right" => "Right".into(),
        "up" => "Up".into(),
        "down" => "Down".into(),
        "pageup" | "page_up" => "Page_Up".into(),
        "pagedown" | "page_down" => "Page_Down".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        k if k.len() > 1 && k.starts_with('f') && k[1..].parse::<u8>().is_ok() => {
            k.to_uppercase()
        }
        k if k.chars().count() == 1 => k.to_string(),
        k => gdk::Key::from_name(k).map(|_| k.to_string())?,
    };
    let mut accel = String::new();
    if ctrl {
        accel.push_str("<Control>");
    }
    if shift {
        accel.push_str("<Shift>");
    }
    if alt {
        accel.push_str("<Alt>");
    }
    if sup {
        accel.push_str("<Super>");
    }
    accel.push_str(&name);
    parse_accel(&accel).map(|_| accel)
}

/// Accelerators per action: the defaults with `[keybindings]` applied (a binding to `"none"`
/// or `""` removes that combination from every action). Unknown names become warnings.
pub fn bindings(
    user: &std::collections::BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) -> HashMap<&'static str, Vec<String>> {
    let mut out: HashMap<&'static str, Vec<String>> = ACTIONS
        .iter()
        .map(|a| (a.name, a.accels.iter().map(|s| s.to_string()).collect()))
        .collect();
    for (spec, action) in user {
        let Some(accel) = parse_binding(spec) else {
            warnings.push(format!("keybindings: cannot read \"{spec}\""));
            continue;
        };
        let action = action.trim();
        let target = if action.is_empty() || action == "none" {
            None
        } else {
            match find(action) {
                Some(def) => Some(def.name),
                None => {
                    // A typo leaves the combination's default alone.
                    warnings.push(format!("keybindings: unknown action \"{action}\""));
                    continue;
                }
            }
        };
        let parsed = parse_accel(&accel);
        // The combination now means only this action (or nothing).
        for list in out.values_mut() {
            list.retain(|a| parse_accel(a) != parsed);
        }
        if let Some(name) = target {
            out.entry(name).or_default().insert(0, accel);
        }
    }
    out
}

/// Combination → action, for the terminal's own key handling.
pub fn keymap(bindings: &HashMap<&'static str, Vec<String>>) -> HashMap<Combo, &'static str> {
    let mut map = HashMap::new();
    for (name, accels) in bindings {
        for accel in accels {
            if let Some((key, mods)) = parse_accel(accel) {
                map.insert(combo(key, mods), *name);
            }
        }
    }
    map
}

/// "Ctrl+Shift+T" for the palette and tooltips.
pub fn label(accel: &str) -> String {
    parse_accel(accel)
        .map(|(k, m)| gtk::accelerator_get_label(k, m).to_string())
        .unwrap_or_default()
}

use gtk::glib::translate::IntoGlib;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bindings() {
        assert_eq!(parse_binding("ctrl+shift+d").as_deref(), Some("<Control><Shift>d"));
        assert_eq!(parse_binding("cmd+d").as_deref(), Some("<Control><Shift>d"));
        assert_eq!(parse_binding("cmd+shift+d").as_deref(), Some("<Control><Shift><Alt>d"));
        assert_eq!(parse_binding("alt+1").as_deref(), Some("<Alt>1"));
        assert_eq!(parse_binding("f11").as_deref(), Some("F11"));
        assert_eq!(parse_binding("ctrl+enter").as_deref(), Some("<Control>Return"));
        assert_eq!(parse_binding("ctrl++").as_deref(), Some("<Control>plus"));
        assert_eq!(parse_binding("hyper+x"), None);
    }

    #[test]
    fn user_bindings_replace_defaults() {
        let mut user = std::collections::BTreeMap::new();
        user.insert("ctrl+shift+d".to_string(), "split_down".to_string());
        user.insert("ctrl+shift+t".to_string(), "none".to_string());
        user.insert("ctrl+x".to_string(), "bogus".to_string());
        // A typo keeps the default: ctrl+shift+w still closes the pane.
        user.insert("ctrl+shift+w".to_string(), "close_pnae".to_string());
        let mut warnings = Vec::new();
        let b = bindings(&user, &mut warnings);
        assert_eq!(b["split_down"][0], "<Control><Shift>d");
        assert!(!b["split_right"].contains(&"<Control><Shift>d".to_string()));
        assert!(b["new_tab"].is_empty());
        assert!(b.values().flatten().any(|a| a == "<Control><Shift>w"));
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn every_action_is_unique() {
        let mut names: Vec<&str> = ACTIONS.iter().map(|a| a.name).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len());
        for a in ACTIONS {
            for accel in a.accels {
                assert!(parse_accel(accel).is_some(), "{accel}");
            }
        }
    }
}
