//! The session (SessionManager.swift + Workspaces.swift): daemon connections, the window's
//! tabs and splits, workspaces, layout persistence, the daemon's events and every app action.

use std::cell::{Cell, Ref, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::{Value, json};
use thurm_proto::layout::{Layout, TabLayout, WindowLayout, Workspace as WorkspaceLayout};
use thurm_proto::{AgentStatus, PaneId, PaneInfo};

use crate::actions::{self, ACTIONS, Combo};
use crate::config::UiConfig;
use crate::core::{self, Core, HostId, LOCAL, PaneKey};
use crate::dialogs;
use crate::model::{self, Axis, Direction, SplitNode, Workspace};
use crate::palette::{self, Item};
use crate::render::Fonts;
use crate::sidebar::{AgentRow, Group, TabRow};
use crate::tab::Tab;
use crate::view::{self, TermView};
use crate::window::MainWindow;
use crate::{integrations, notify, processes, quick, remote};

pub const APP_ID: &str = "rs.thurm.Thurm";

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
    static UPGRADE_TRIED: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` with the application (main thread only); `None` before it started.
pub fn with_app<R>(f: impl FnOnce(&Rc<App>) -> R) -> Option<R> {
    let app = APP.with(|a| a.borrow().clone());
    app.map(|a| f(&a))
}

pub struct App {
    pub gtk_app: adw::Application,
    cores: RefCell<HashMap<HostId, Rc<Core>>>,
    epochs: RefCell<HashMap<HostId, u64>>,
    ui: RefCell<UiConfig>,
    fonts: RefCell<Rc<Fonts>>,
    font_override: Cell<Option<f64>>,
    keymap: RefCell<HashMap<Combo, &'static str>>,
    pub infos: RefCell<HashMap<PaneKey, PaneInfo>>,
    views: RefCell<HashMap<PaneKey, Rc<TermView>>>,
    pub window: RefCell<Option<Rc<MainWindow>>>,
    pub workspaces: RefCell<Vec<Workspace>>,
    seen_waiting: RefCell<HashSet<PaneKey>>,
    last_layout: RefCell<String>,
    save_scheduled: Cell<bool>,
    session_ready: Cell<bool>,
    terminating: Cell<bool>,
    reconnect_attempt: Cell<u32>,
    reconnecting: Cell<bool>,
    sidebar_refresh: Cell<bool>,
    collapsed_groups: RefCell<Vec<String>>,
    menu_tab: RefCell<Option<std::rc::Weak<Tab>>>,
    last_dark: Cell<Option<bool>>,
    pub remotes: remote::Remotes,
    pub quick: RefCell<Option<Rc<quick::QuickTerminal>>>,
    /// The stored quick-terminal tab, until the quick terminal is first shown.
    pub quick_layout: RefCell<Option<TabLayout>>,
}

pub fn run() -> std::process::ExitCode {
    // X11 window class = program name: match the desktop entry (Wayland uses the app id).
    glib::set_prgname(Some(APP_ID));
    integrations::register_fonts();
    let gtk_app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    gtk_app.add_main_option(
        "quick-terminal",
        glib::Char::from(b'q'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Show or hide the quick terminal",
        None,
    );
    for (name, help) in [("new-tab", "Open a new tab"), ("new-workspace", "Open a new workspace")] {
        gtk_app.add_main_option(name, glib::Char::from(0), glib::OptionFlags::NONE, glib::OptionArg::None, help, None);
    }
    gtk_app.connect_command_line(|gtk_app, cmd| {
        let opts = cmd.options_dict();
        let quick = opts.contains("quick-terminal");
        let new_tab = opts.contains("new-tab");
        let new_workspace = opts.contains("new-workspace");
        if with_app(|_| ()).is_none() {
            match App::start(gtk_app) {
                Ok(()) => {}
                Err(ConnectError::Failed(e)) => {
                    fatal(gtk_app, &e);
                    return 0.into();
                }
                Err(ConnectError::OldDaemon(e)) => {
                    ask_restart_daemon(gtk_app, &e);
                    return 0.into();
                }
            }
        }
        with_app(|a| {
            if quick {
                a.toggle_quick();
                return;
            }
            if new_tab {
                a.new_tab(None, None, None);
            } else if new_workspace {
                a.new_workspace(LOCAL);
            }
            if let Some(w) = a.win() {
                w.window.present();
            }
        });
        0.into()
    });
    gtk_app.connect_shutdown(|_| {
        with_app(|a| a.prepare_for_termination());
        APP.with(|a| a.borrow_mut().take());
    });
    let code = gtk_app.run();
    std::process::ExitCode::from(code.get())
}

/// The running daemon is too old to replace in place: restarting it ends the programs in its
/// panes, so the user decides (like the macOS app).
fn ask_restart_daemon(gtk_app: &adw::Application, detail: &str) {
    log::warn!("{detail}");
    let window = adw::ApplicationWindow::new(gtk_app);
    window.set_title(Some("Thurm"));
    window.set_default_size(640, 420);
    window.present();
    let dialog = adw::AlertDialog::new(
        Some("Restart the session daemon?"),
        Some(
            "Thurm was updated, but the session daemon still runs the previous version. \
             Restarting it closes the programs running in your tabs; tabs, splits, scrollback \
             and working directories come back.",
        ),
    );
    dialog.add_response("quit", "Quit");
    dialog.add_response("restart", "Restart Daemon");
    dialog.set_response_appearance("restart", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("quit"));
    dialog.set_close_response("quit");
    let app = gtk_app.clone();
    let w = window.clone();
    dialog.connect_response(None, move |_, response| {
        w.close();
        if response != "restart" {
            return;
        }
        if !core::terminate_daemon() {
            log::warn!("could not stop the old daemon");
        }
        match App::start(&app) {
            Ok(()) => {
                if let Some(win) = with_app(|a| a.win()).flatten() {
                    win.window.present();
                }
            }
            Err(ConnectError::Failed(e) | ConnectError::OldDaemon(e)) => fatal(&app, &e),
        }
    });
    dialog.present(Some(&window));
}

fn fatal(gtk_app: &adw::Application, message: &str) {
    log::error!("{message}");
    let window = adw::ApplicationWindow::new(gtk_app);
    window.set_title(Some("Thurm"));
    window.set_default_size(640, 420);
    window.present();
    let dialog = adw::AlertDialog::new(
        Some("Cannot connect to the Thurm session daemon"),
        Some(message),
    );
    dialog.add_response("quit", "Quit");
    let w = window.clone();
    dialog.connect_response(None, move |_, _| w.close());
    dialog.present(Some(&window));
}

/// The desktop prefers dark (GNOME's color-scheme, else the GTK theme's name).
pub fn system_is_dark() -> bool {
    if let Some(source) = gio::SettingsSchemaSource::default()
        && let Some(schema) = source.lookup("org.gnome.desktop.interface", true)
        && schema.has_key("color-scheme")
    {
        return gio::Settings::new("org.gnome.desktop.interface").string("color-scheme")
            == "prefer-dark";
    }
    gtk::Settings::default().is_some_and(|s| {
        s.is_gtk_application_prefer_dark_theme()
            || s.gtk_theme_name().is_some_and(|n| n.to_lowercase().contains("dark"))
    })
}

pub enum ConnectError {
    Failed(String),
    /// A daemon on another protocol that can't be replaced in place: only restarting it (which
    /// ends its programs) helps.
    OldDaemon(String),
}

/// Connects and says Hello; replaces an older daemon in place (once per launch). Never stops
/// the daemon.
fn connect_local(epoch: u64, upgraded: &Cell<bool>) -> Result<Core, ConnectError> {
    let daemon = core::daemon_path();
    let hello =
        json!({"Hello": {"client": "thurm-gtk", "version": core::protocol_version(), "ui": true}});
    loop {
        let core = match Core::connect(daemon.as_deref(), epoch) {
            Ok(c) => c,
            Err(e) if e.contains("protocol mismatch") => {
                if !upgraded.get() && let Some(path) = daemon.as_deref() {
                    upgraded.set(true);
                    let rc = core::upgrade_daemon(path);
                    log::info!("in-place daemon upgrade: {rc}");
                    if rc == 0 {
                        continue;
                    }
                }
                return Err(ConnectError::OldDaemon(format!(
                    "The running thurmd speaks another protocol: {e}. Run `thurm daemon stop` (layout and scrollback are restored), then open Thurm again."
                )));
            }
            Err(e) => return Err(ConnectError::Failed(e)),
        };
        let resp = core.request(&hello);
        if let Some(e) = resp.get("error").and_then(Value::as_str) {
            return Err(ConnectError::Failed(format!(
                "The running thurmd refused the connection: {e}. Quit Thurm and run `thurm daemon stop` (layout and scrollback are restored), then open Thurm again."
            )));
        }
        let build = resp.pointer("/Hello/build").and_then(Value::as_str).unwrap_or("");
        if !upgraded.get()
            && !build.is_empty()
            && build != core::build_id()
            && let Some(path) = daemon.as_deref()
        {
            upgraded.set(true);
            log::info!("thurmd runs build {build}, this app {}", core::build_id());
            drop(core);
            if core::upgrade_daemon(path) == 0 {
                log::info!("thurmd upgraded in place");
            }
            continue;
        }
        return Ok(core);
    }
}

impl App {
    fn start(gtk_app: &adw::Application) -> Result<(), ConnectError> {
        integrations::register_icons();
        let dark = system_is_dark();
        let ui = UiConfig::load(dark);
        let core = UPGRADE_TRIED.with(|u| connect_local(1, u))?;
        core.send(&json!({"SetAppearance": {"dark": dark}}));
        let window = MainWindow::new(gtk_app, &ui);
        let fonts = Rc::new(Fonts::new(
            &window.window.pango_context(),
            &ui.cfg,
            &ui.font_features,
            ui.font_size(),
        ));
        let mut warnings = Vec::new();
        let bindings = actions::bindings(&ui.cfg.keybindings, &mut warnings);
        let app = Rc::new(App {
            gtk_app: gtk_app.clone(),
            cores: RefCell::new(HashMap::from([(LOCAL.to_string(), Rc::new(core))])),
            epochs: RefCell::new(HashMap::from([(LOCAL.to_string(), 1)])),
            ui: RefCell::new(ui),
            fonts: RefCell::new(fonts),
            font_override: Cell::new(None),
            keymap: RefCell::new(actions::keymap(&bindings)),
            infos: RefCell::new(HashMap::new()),
            views: RefCell::new(HashMap::new()),
            window: RefCell::new(Some(window.clone())),
            workspaces: RefCell::new(Vec::new()),
            seen_waiting: RefCell::new(HashSet::new()),
            last_layout: RefCell::new(String::new()),
            save_scheduled: Cell::new(false),
            session_ready: Cell::new(false),
            terminating: Cell::new(false),
            reconnect_attempt: Cell::new(0),
            reconnecting: Cell::new(false),
            sidebar_refresh: Cell::new(false),
            collapsed_groups: RefCell::new(integrations::load_state_list("collapsed_groups")),
            menu_tab: RefCell::new(None),
            last_dark: Cell::new(Some(dark)),
            remotes: remote::Remotes::default(),
            quick: RefCell::new(None),
            quick_layout: RefCell::new(None),
        });
        app.ui.borrow_mut().warnings.extend(warnings);
        APP.with(|a| *a.borrow_mut() = Some(app.clone()));
        app.register_actions(&bindings);
        notify::install(&app);
        app.restore_session();
        window.window.present();
        app.watch_appearance();
        remote::start(&app);
        quick::configure(&app);
        // A safety net: the layout is also saved every 15 s when it changed.
        glib::timeout_add_seconds_local(15, || {
            with_app(|a| a.save_now(false));
            glib::ControlFlow::Continue
        });
        let problem = app.ui().problem();
        if let Some(p) = problem {
            glib::timeout_add_local_once(Duration::from_millis(500), move || {
                with_app(|a| a.toast(&p, 8.0));
            });
        }
        Ok(())
    }

    // MARK: accessors

    pub fn ui(&self) -> Ref<'_, UiConfig> {
        self.ui.borrow()
    }

    pub fn fonts(&self) -> Rc<Fonts> {
        self.fonts.borrow().clone()
    }

    pub fn core(&self, host: &str) -> Option<Rc<Core>> {
        self.cores.borrow().get(host).cloned()
    }

    pub fn is_connected(&self, host: &str) -> bool {
        self.cores.borrow().contains_key(host)
    }

    pub fn connected_remotes(&self) -> Vec<HostId> {
        let mut v: Vec<HostId> = self
            .cores
            .borrow()
            .keys()
            .filter(|h| *h != LOCAL)
            .cloned()
            .collect();
        v.sort();
        v
    }

    pub fn win(&self) -> Option<Rc<MainWindow>> {
        self.window.borrow().clone()
    }

    pub fn view(&self, key: &PaneKey) -> Option<Rc<TermView>> {
        self.views.borrow().get(key).cloned()
    }

    pub fn views(&self) -> Vec<Rc<TermView>> {
        self.views.borrow().values().cloned().collect()
    }

    pub fn ensure_view(&self, key: &PaneKey) -> Rc<TermView> {
        if let Some(v) = self.view(key) {
            return v;
        }
        let v = TermView::new(key.clone(), self.fonts());
        if key.is_remote() {
            v.set_offline(self.remotes.offline_message(self, &key.host));
        }
        if let Some(info) = self.infos.borrow().get(key) {
            v.set_progress(info.progress);
        }
        self.views.borrow_mut().insert(key.clone(), v.clone());
        v
    }

    pub fn pane_info(&self, key: &PaneKey) -> Option<PaneInfo> {
        self.infos.borrow().get(key).cloned()
    }

    pub fn divider_color(&self) -> u32 {
        let ui = self.ui();
        crate::config::blend(ui.theme.background, ui.theme.foreground, 0.25)
    }

    /// All tabs: the window's and the quick terminal's.
    fn all_tabs(&self) -> Vec<Rc<Tab>> {
        let mut tabs = self.win().map(|w| w.ordered_tabs()).unwrap_or_default();
        if let Some(q) = self.quick.borrow().as_ref() {
            tabs.push(q.tab.clone());
        }
        tabs
    }

    pub fn tab_of(&self, key: &PaneKey) -> Option<Rc<Tab>> {
        self.all_tabs().into_iter().find(|t| t.contains(key))
    }

    pub fn current_tab(&self) -> Option<Rc<Tab>> {
        if let Some(q) = self.quick.borrow().as_ref()
            && q.is_active()
        {
            return Some(q.tab.clone());
        }
        self.win().and_then(|w| w.selected_tab())
    }

    pub fn focused_pane(&self) -> Option<PaneKey> {
        self.current_tab().map(|t| t.focused.borrow().clone())
    }

    pub fn focused_view(&self) -> Option<Rc<TermView>> {
        self.focused_pane().and_then(|k| self.view(&k))
    }

    /// The window's workspace.
    pub fn current_workspace(&self) -> Option<u64> {
        let w = self.win()?;
        let tab = w.selected_tab().or_else(|| w.ordered_tabs().into_iter().next())?;
        Some(tab.workspace.get()).filter(|id| *id != 0)
    }

    pub fn workspace(&self, id: u64) -> Option<Workspace> {
        self.workspaces.borrow().iter().find(|w| w.id == id).cloned()
    }

    /// The daemon new panes of the window go to.
    pub fn current_host(&self) -> HostId {
        if let Some(q) = self.quick.borrow().as_ref()
            && q.is_active()
        {
            return LOCAL.into();
        }
        self.current_workspace()
            .and_then(|id| self.workspace(id))
            .map(|w| w.host)
            .or_else(|| self.current_tab().map(|t| t.host.clone()))
            .unwrap_or_else(|| LOCAL.into())
    }

    pub fn toast(&self, text: &str, secs: f64) {
        if let Some(v) = self.focused_view() {
            v.show_toast(text, secs);
        }
    }

    // MARK: keyboard

    pub fn action_for_key(
        &self,
        keyval: gdk::Key,
        keycode: u32,
        state: gdk::ModifierType,
    ) -> Option<&'static str> {
        let map = self.keymap.borrow();
        if let Some(a) = map.get(&actions::combo(keyval, state)) {
            return Some(a);
        }
        // Shifted punctuation (Ctrl+Shift+= arrives as "plus"): try the key's unshifted symbol.
        let display = gdk::Display::default()?;
        let (base, ..) = gdk::prelude::DisplayExtManual::translate_key(
            &display,
            keycode,
            gdk::ModifierType::empty(),
            0,
        )?;
        map.get(&actions::combo(base, state)).copied()
    }

    pub fn activate(&self, name: &str) {
        self.gtk_app.activate_action(name, None);
    }

    // MARK: actions

    fn register_actions(self: &Rc<Self>, bindings: &HashMap<&'static str, Vec<String>>) {
        for def in ACTIONS {
            let action = gio::SimpleAction::new(def.name, None);
            let name = def.name;
            action.connect_activate(move |_, _| {
                with_app(|a| a.run_action(name));
            });
            self.gtk_app.add_action(&action);
            let accels: Vec<&str> = bindings
                .get(def.name)
                .map(|v| v.iter().map(String::as_str).collect())
                .unwrap_or_default();
            self.gtk_app
                .set_accels_for_action(&format!("app.{}", def.name), &accels);
        }
        let theme = gio::SimpleAction::new("theme", Some(glib::VariantTy::STRING));
        theme.connect_activate(|_, p| {
            if let Some(name) = p.and_then(|v| v.str().map(str::to_string)) {
                with_app(|a| a.choose_theme(&name));
            }
        });
        self.gtk_app.add_action(&theme);
        let follow = gio::SimpleAction::new("follow_appearance_toggle", None);
        follow.connect_activate(|_, _| {
            with_app(|a| a.toggle_follow_appearance());
        });
        self.gtk_app.add_action(&follow);
        let ws = gio::SimpleAction::new("switch_to_workspace", Some(glib::VariantTy::UINT64));
        ws.connect_activate(|_, p| {
            if let Some(id) = p.and_then(|v| v.get::<u64>()) {
                with_app(|a| a.switch_workspace(id));
            }
        });
        self.gtk_app.add_action(&ws);
        let close_menu_tab = gio::SimpleAction::new("close_menu_tab", None);
        close_menu_tab.connect_activate(|_, _| {
            with_app(|a| {
                let tab = a.menu_tab.borrow_mut().take().and_then(|w| w.upgrade());
                if let Some(t) = tab {
                    a.close_tab(&t);
                }
            });
        });
        self.gtk_app.add_action(&close_menu_tab);
        let move_menu_tab = gio::SimpleAction::new("move_menu_tab", None);
        move_menu_tab.connect_activate(|_, _| {
            with_app(|a| {
                let tab = a.menu_tab.borrow_mut().take().and_then(|w| w.upgrade());
                if let Some(t) = tab {
                    a.move_tab_to_new_workspace(&t);
                }
            });
        });
        self.gtk_app.add_action(&move_menu_tab);
        integrations::register_actions(self);
        self.update_action_states();
    }

    /// Enables the actions that depend on state (menus and the palette leave disabled ones out).
    pub fn update_action_states(&self) {
        let has_ws = self.current_workspace().is_some();
        let sidebar = self.ui().sidebar_tabs();
        let explain = self.ui().ai_explain();
        for (name, on) in [
            ("rename_workspace", has_ws),
            ("close_workspace", has_ws),
            ("toggle_sidebar", sidebar),
            ("explain", explain),
        ] {
            if let Some(a) = self
                .gtk_app
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                a.set_enabled(on);
            }
        }
    }

    pub fn action_visible(&self, name: &str) -> bool {
        self.gtk_app
            .lookup_action(name)
            .is_some_and(|a| a.is_enabled())
            && name != "command_palette"
    }

    /// Titles that follow state ("Show Sidebar", a checkmark on "Tabs in Sidebar").
    pub fn action_label(&self, name: &str) -> String {
        match name {
            "toggle_sidebar" => {
                if self.win().is_some_and(|w| w.sidebar_shown()) {
                    "Hide Sidebar".into()
                } else {
                    "Show Sidebar".into()
                }
            }
            "toggle_tab_style" => {
                if self.ui().sidebar_tabs() {
                    "✓ Tabs in Sidebar".into()
                } else {
                    "Tabs in Sidebar".into()
                }
            }
            _ => actions::find(name).map_or(name.to_string(), |d| d.label.to_string()),
        }
    }

    fn run_action(self: &Rc<Self>, name: &str) {
        let focused = self.focused_pane();
        let tab = self.current_tab();
        match name {
            "about" => self.about(),
            "open_config" | "open_config_file" => self.open_config_file(),
            "reload_config" => self.reload_config(true),
            "remotes" => remote::show_window(self),
            "quit" => self.quit(),
            "new_tab" => {
                self.new_tab(None, None, None);
            }
            "new_workspace" => self.new_workspace(LOCAL),
            "switch_workspace" => self.show_workspace_switcher(),
            "switch_agent" => self.show_agent_picker(),
            "rename_workspace" => {
                if let Some(id) = self.current_workspace() {
                    self.rename_workspace(id, false);
                }
            }
            "close_workspace" => {
                if let Some(id) = self.current_workspace() {
                    self.close_workspace(id);
                }
            }
            "split_right" => self.split_focused(Direction::Right, None, false),
            "split_down" => self.split_focused(Direction::Down, None, false),
            "command_palette" => self.show_command_palette(),
            "processes" => processes::show(self),
            "close_pane" => {
                if let Some(k) = focused {
                    self.user_close_pane(&k);
                }
            }
            "close_tab" => {
                if let Some(t) = tab {
                    self.close_tab(&t);
                }
            }
            "copy" => {
                if let Some(k) = focused {
                    self.copy(&k);
                }
            }
            "paste" => {
                if let Some(k) = focused {
                    self.paste(&k);
                }
            }
            "select_all" => self.pane_send(focused, |id| {
                json!({"Selection": {"pane": id, "op": "SelectAll"}})
            }),
            "clear_screen" => self.pane_send(focused, |id| json!({"ClearScreen": {"pane": id}})),
            "clear_scrollback" => {
                self.pane_send(focused, |id| json!({"ClearScrollback": {"pane": id}}))
            }
            "explain" => self.explain(),
            "find" => {
                if let Some(t) = tab {
                    t.show_find();
                }
            }
            "find_next" | "find_previous" => {
                if let Some(t) = tab {
                    if !t.find_visible() {
                        t.show_find();
                    }
                    t.search(if name == "find_next" { "Forward" } else { "Backward" });
                }
            }
            "increase_font_size" => self.change_font(1.0),
            "decrease_font_size" => self.change_font(-1.0),
            "reset_font_size" => self.change_font(0.0),
            "toggle_tab_style" => self.toggle_tab_style(),
            "toggle_sidebar" => {
                if let Some(w) = self.win() {
                    w.toggle_sidebar();
                }
            }
            "browse_themes" => self.browse_themes(),
            "follow_appearance" => self.toggle_follow_appearance(),
            "zoom_split" => self.toggle_zoom(),
            "equalize_splits" => {
                if let Some(t) = tab {
                    t.tree.borrow_mut().equalize();
                    t.relayout();
                    self.schedule_save();
                }
            }
            "focus_left" => self.move_focus(Direction::Left),
            "focus_right" => self.move_focus(Direction::Right),
            "focus_up" => self.move_focus(Direction::Up),
            "focus_down" => self.move_focus(Direction::Down),
            "resize_left" => self.resize_focused(Axis::Horizontal, -0.05),
            "resize_right" => self.resize_focused(Axis::Horizontal, 0.05),
            "resize_up" => self.resize_focused(Axis::Vertical, -0.05),
            "resize_down" => self.resize_focused(Axis::Vertical, 0.05),
            "toggle_fullscreen" => {
                if let Some(w) = self.win() {
                    w.window.set_fullscreened(!w.window.is_fullscreen());
                }
            }
            "minimize" => {
                if let Some(w) = self.win() {
                    w.window.minimize();
                }
            }
            "maximize" => {
                if let Some(w) = self.win() {
                    w.window.set_maximized(!w.window.is_maximized());
                }
            }
            "previous_tab" | "next_tab" => self.cycle_tab(name == "next_tab"),
            "select_last_tab" => self.select_tab_number(9),
            "move_tab_to_new_workspace" => {
                if let Some(t) = tab {
                    self.move_tab_to_new_workspace(&t);
                }
            }
            "quick_terminal" => self.toggle_quick(),
            n if n.starts_with("select_tab_") => {
                if let Ok(i) = n["select_tab_".len()..].parse() {
                    self.select_tab_number(i);
                }
            }
            n if n.starts_with("workspace_") => {
                if let Ok(i) = n["workspace_".len()..].parse::<usize>() {
                    let ids: Vec<u64> =
                        self.workspaces_by_recency().iter().map(|w| w.id).collect();
                    if let Some(&id) = ids.get(i - 1) {
                        self.switch_workspace(id);
                    }
                }
            }
            _ => log::warn!("unhandled action {name}"),
        }
    }

    fn pane_send(&self, key: Option<PaneKey>, req: impl Fn(PaneId) -> Value) {
        if let Some(k) = key
            && let Some(c) = self.core(&k.host)
        {
            c.send(&req(k.id));
            if let Some(v) = self.view(&k) {
                v.area.queue_draw();
            }
        }
    }

    fn about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("Thurm")
            .application_icon(APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .comments("A terminal for persistent sessions, coding agents, and remote work.")
            .website("https://thurm.rs")
            .license_type(gtk::License::Apache20)
            .build();
        if let Some(w) = self.win() {
            about.present(Some(&w.window));
        }
    }

    fn open_config_file(&self) {
        let path = thurm_config::config_path();
        if !path.exists() {
            let _ = thurm_config::ensure_default_config();
        }
        let uri = gio::File::for_path(&path).uri();
        let _ = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
    }

    // MARK: session

    fn restore_session(self: &Rc<Self>) {
        let Some(core) = self.core(LOCAL) else { return };
        let panes = core.request(&json!("ListPanes"));
        let mut alive = HashSet::new();
        {
            let mut infos = self.infos.borrow_mut();
            infos.retain(|k, _| k.is_remote());
            for p in panes.get("Panes").and_then(Value::as_array).into_iter().flatten() {
                if let Ok(info) = serde_json::from_value::<PaneInfo>(p.clone()) {
                    if info.alive {
                        alive.insert(info.id);
                    }
                    infos.insert(PaneKey::local(info.id), info);
                }
            }
        }
        let mut layout = core
            .request(&json!("GetLayout"))
            .get("Layout")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Layout>(s).ok())
            .unwrap_or_default();
        let remote_names = self.ui().remote_names();
        let placed: HashSet<PaneKey> = self.all_tabs().iter().flat_map(|t| t.panes()).collect();
        let keep = |host: Option<&str>, id: PaneId| -> bool {
            let key = PaneKey::new(host.unwrap_or(LOCAL), id);
            if placed.contains(&key) {
                return false;
            }
            match host {
                None | Some(LOCAL) => alive.contains(&id),
                Some(h) => remote_names.iter().any(|n| n == h),
            }
        };
        retain_layout(&mut layout, &keep);

        {
            let mut wss = self.workspaces.borrow_mut();
            wss.clear();
            for w in &layout.workspaces {
                wss.push(Workspace {
                    id: w.id,
                    name: if w.name.is_empty() {
                        format!("workspace-{}", w.id)
                    } else {
                        w.name.clone()
                    },
                    last_active: w.last_active,
                    hidden_tabs: w.tabs.clone(),
                    hidden_selected: w.selected_tab,
                    host: w.host.clone().unwrap_or_else(|| LOCAL.into()),
                });
            }
        }
        *self.quick_layout.borrow_mut() = layout.quick.clone();
        let mut used: HashSet<PaneKey> = HashSet::new();
        let mut mark = |tabs: &[TabLayout], host: &str| {
            for t in tabs {
                for (h, id) in leaves(&t.root) {
                    used.insert(PaneKey::new(h.unwrap_or(host), id));
                }
            }
        };
        for ws in self.workspaces.borrow().iter() {
            mark(&ws.hidden_tabs, &ws.host);
        }
        if let Some(q) = &layout.quick {
            mark(std::slice::from_ref(q), LOCAL);
        }
        for w in &layout.windows {
            mark(&w.tabs, LOCAL);
        }
        let mut shown_window = false;
        for w in &layout.windows {
            if !shown_window {
                shown_window = self.restore_window(w);
            } else {
                self.hide_window_layout(w);
            }
        }
        // Panes no layout holds: one background workspace with a tab each.
        let mut orphans: Vec<PaneId> = alive
            .iter()
            .copied()
            .filter(|id| {
                !used.contains(&PaneKey::local(*id)) && !placed.contains(&PaneKey::local(*id))
            })
            .collect();
        orphans.sort();
        if !orphans.is_empty() {
            let mut ws = self.make_workspace(LOCAL);
            ws.hidden_tabs = orphans
                .iter()
                .map(|id| TabLayout {
                    title: None,
                    root: thurm_proto::layout::LayoutNode::local(*id),
                    focused: *id,
                    zoomed: None,
                    handoff: None,
                })
                .collect();
            self.workspaces.borrow_mut().push(ws);
        }
        if !shown_window {
            self.default_window_size();
            let recent = self
                .workspaces_by_recency()
                .into_iter()
                .find(|w| !w.hidden_tabs.is_empty());
            match recent {
                Some(ws) => self.show_workspace(ws.id),
                None => {
                    let ws = self.make_workspace(LOCAL);
                    let id = ws.id;
                    self.workspaces.borrow_mut().push(ws);
                    self.show_workspace(id);
                }
            }
        }
        self.session_ready.set(true);
        self.save_now(true);
        self.focus_changed();
        if let Some(t) = self.current_tab() {
            let k = t.focused.borrow().clone();
            self.focus_pane(&k);
        }
    }

    /// The stored window becomes the window; false when it had no tabs left.
    fn restore_window(self: &Rc<Self>, w: &WindowLayout) -> bool {
        if w.tabs.is_empty() {
            return false;
        }
        let Some(win) = self.win() else { return false };
        let ws_id = match self.workspace(w.workspace) {
            Some(ws) if ws.hidden_tabs.is_empty() => ws.id,
            _ => {
                let ws = self.make_workspace(LOCAL);
                let id = ws.id;
                self.workspaces.borrow_mut().push(ws);
                id
            }
        };
        match w.frame {
            Some(f) if f[2] > 50.0 && f[3] > 50.0 => {
                win.window.set_default_size(f[2] as i32, f[3] as i32);
                win.frame_origin.set((f[0], f[1]));
            }
            _ => self.default_window_size(),
        }
        let host = self
            .workspace(ws_id)
            .map(|w| w.host)
            .unwrap_or_else(|| LOCAL.into());
        let selected = w.selected_tab.min(w.tabs.len() - 1);
        let mut chosen = None;
        for (i, t) in w.tabs.iter().enumerate() {
            if let Some(tab) = Tab::from_layout(t, &host, ws_id) {
                win.add_tab(&tab, false);
                tab.sync_children();
                if i == selected || chosen.is_none() {
                    chosen = Some(tab);
                }
            }
        }
        if let Some(t) = chosen {
            win.select_tab(&t);
        }
        if w.fullscreen {
            win.window.fullscreen();
        }
        if let Some(ws) = self.workspaces.borrow_mut().iter_mut().find(|x| x.id == ws_id) {
            ws.last_active = model::now_secs();
        }
        true
    }

    /// Older layouts with several windows: the others become hidden workspaces.
    fn hide_window_layout(&self, w: &WindowLayout) {
        let existing = self
            .workspace(w.workspace)
            .filter(|ws| ws.hidden_tabs.is_empty() && !self.is_shown(ws.id))
            .map(|ws| ws.id);
        if let Some(id) = existing
            && let Some(ws) = self.workspaces.borrow_mut().iter_mut().find(|x| x.id == id)
        {
            ws.hidden_tabs = w.tabs.clone();
            ws.hidden_selected = w.selected_tab;
            return;
        }
        let mut ws = self.make_workspace(LOCAL);
        ws.hidden_tabs = w.tabs.clone();
        ws.hidden_selected = w.selected_tab;
        self.workspaces.borrow_mut().push(ws);
    }

    fn default_window_size(&self) {
        let Some(win) = self.win() else { return };
        let ui = self.ui();
        let f = self.fonts();
        let (px, py) = ui.padding();
        let cols = ui.cfg.window.columns.clamp(10, 1000) as f64;
        let rows = ui.cfg.window.rows.clamp(3, 1000) as f64;
        let w = (cols * f.cell_w + 2.0 * px + 1.0).ceil();
        let h = (rows * f.cell_h + 2.0 * py + 1.0).ceil() + 47.0;
        let side = if ui.sidebar_tabs() { ui.sidebar_width() } else { 0.0 };
        win.window.set_default_size((w + side) as i32, h as i32);
    }

    pub fn layout(&self) -> Layout {
        let mut layout = Layout::default();
        if let Some(win) = self.win() {
            let ordered = win.ordered_tabs();
            let tabs: Vec<TabLayout> = ordered.iter().map(|t| t.to_layout()).collect();
            let selected = win
                .selected_tab()
                .and_then(|s| ordered.iter().position(|t| Rc::ptr_eq(t, &s)))
                .unwrap_or(0);
            let (x, y) = win.frame_origin.get();
            let (w, h) = (win.window.width(), win.window.height());
            if !tabs.is_empty() {
                layout.windows.push(WindowLayout {
                    frame: (w > 0 && h > 0).then_some([x, y, w as f64, h as f64]),
                    tabs,
                    selected_tab: selected,
                    fullscreen: win.window.is_fullscreen(),
                    workspace: self.current_workspace().unwrap_or(0),
                });
            }
        }
        let shown = self.current_workspace();
        let infos = self.infos.borrow();
        for ws in self.workspaces.borrow().iter() {
            let hidden = Some(ws.id) != shown;
            let mut tabs = ws.hidden_tabs.clone();
            if hidden {
                let host = ws.host.clone();
                let known = |h: Option<&str>, id: PaneId| {
                    let key = PaneKey::new(h.unwrap_or(&host), id);
                    infos.contains_key(&key) || (key.is_remote() && !self.is_connected(&key.host))
                };
                retain_tabs(&mut tabs, &known);
                if tabs.is_empty() {
                    continue;
                }
            } else {
                tabs.clear();
            }
            layout.workspaces.push(WorkspaceLayout {
                id: ws.id,
                name: ws.name.clone(),
                last_active: ws.last_active,
                selected_tab: if hidden {
                    ws.hidden_selected.min(tabs.len().saturating_sub(1))
                } else {
                    0
                },
                tabs,
                host: ws.is_remote().then(|| ws.host.clone()),
            });
        }
        layout.quick = self
            .quick
            .borrow()
            .as_ref()
            .map(|q| q.tab.to_layout())
            .or_else(|| self.quick_layout.borrow().clone());
        layout
    }

    pub fn save_now(&self, force: bool) {
        if !self.session_ready.get() || self.terminating.get() {
            return;
        }
        let json = sorted_json(&self.layout());
        if !force && *self.last_layout.borrow() == json {
            return;
        }
        if let Some(c) = self.core(LOCAL) {
            c.send(&json!({"SetLayout": {"json": json}}));
            *self.last_layout.borrow_mut() = json;
        }
    }

    pub fn schedule_save(&self) {
        if self.terminating.get() || self.save_scheduled.replace(true) {
            return;
        }
        glib::timeout_add_local_once(Duration::from_millis(500), || {
            with_app(|a| {
                a.save_scheduled.set(false);
                a.save_now(false);
            });
        });
    }

    pub fn quit(&self) {
        self.prepare_for_termination();
        self.gtk_app.quit();
    }

    fn prepare_for_termination(&self) {
        if self.terminating.get() {
            return;
        }
        if self.session_ready.get() {
            let json = sorted_json(&self.layout());
            if let Some(c) = self.core(LOCAL) {
                c.request_timeout(&json!({"SetLayout": {"json": json}}), 3000);
            }
        }
        self.terminating.set(true);
        if matches!(self.ui().cfg.session.quit, thurm_config::QuitBehavior::Terminate)
            && let Some(c) = self.core(LOCAL)
        {
            c.request_timeout(&json!({"Shutdown": {"kill_panes": true}}), 3000);
        }
        for v in self.views.borrow().values() {
            v.detach();
        }
        core::remotes_stop();
        self.cores.borrow_mut().clear();
    }

    // MARK: workspaces

    pub fn make_workspace(&self, host: &str) -> Workspace {
        let wss = self.workspaces.borrow();
        let taken: Vec<String> = wss.iter().map(|w| w.name.clone()).collect();
        Workspace {
            id: wss.iter().map(|w| w.id).max().unwrap_or(0) + 1,
            name: model::workspace_name(&taken, host),
            last_active: model::now_secs(),
            hidden_tabs: Vec::new(),
            hidden_selected: 0,
            host: host.to_string(),
        }
    }

    pub fn workspaces_by_recency(&self) -> Vec<Workspace> {
        let mut v = self.workspaces.borrow().clone();
        v.sort_by_key(|w| std::cmp::Reverse(w.last_active));
        v
    }

    fn is_shown(&self, id: u64) -> bool {
        self.current_workspace() == Some(id)
    }

    pub fn switch_workspace(self: &Rc<Self>, id: u64) {
        if self.current_workspace() == Some(id) {
            return;
        }
        self.show_workspace(id);
    }

    /// Shows workspace `id` in the window; the old tabs go hidden (their shells keep running).
    pub fn show_workspace(self: &Rc<Self>, id: u64) {
        let Some(win) = self.win() else { return };
        let Some(target) = self.workspace(id) else { return };
        let old = win.ordered_tabs();
        let cur = self.current_workspace();
        let mut created = Vec::new();
        for t in &target.hidden_tabs {
            if let Some(tab) = Tab::from_layout(t, &target.host, id) {
                win.add_tab(&tab, false);
                tab.sync_children();
                created.push(tab);
            }
        }
        if created.is_empty()
            && let Some(key) = self.create_pane(&target.host, None, None, None, None, false)
        {
            let tab = Tab::new(SplitNode::Leaf(key.clone()), key, id);
            win.add_tab(&tab, false);
            tab.sync_children();
            created.push(tab);
        }
        if created.is_empty() {
            return;
        }
        // The old workspace keeps its tabs as layouts.
        if let Some(cur) = cur {
            let selected = win.selected_tab();
            let kept: Vec<TabLayout> = old.iter().map(|t| t.to_layout()).collect();
            let sel = selected
                .and_then(|s| old.iter().position(|t| Rc::ptr_eq(t, &s)))
                .unwrap_or(0);
            if let Some(ws) = self.workspaces.borrow_mut().iter_mut().find(|w| w.id == cur) {
                ws.hidden_tabs = kept;
                ws.hidden_selected = sel;
            }
        }
        let sel = created[target.hidden_selected.min(created.len() - 1)].clone();
        win.select_tab(&sel);
        if let Some(ws) = self.workspaces.borrow_mut().iter_mut().find(|w| w.id == id) {
            ws.hidden_tabs.clear();
            ws.hidden_selected = 0;
            ws.last_active = model::now_secs();
        }
        for t in old {
            self.drop_tab_widgets(&t);
            win.remove_tab(&t);
        }
        let k = sel.focused.borrow().clone();
        self.focus_pane(&k);
        self.focus_changed();
        self.schedule_save();
    }

    /// Unsubscribes and forgets the views of a tab leaving the window (panes keep running).
    fn drop_tab_widgets(&self, tab: &Rc<Tab>) {
        for k in tab.panes() {
            let v = self.views.borrow_mut().remove(&k);
            if let Some(v) = v {
                v.detach();
            }
        }
    }

    pub fn new_workspace(self: &Rc<Self>, host: &str) {
        let ws = self.make_workspace(host);
        let (id, name) = (ws.id, ws.name.clone());
        self.workspaces.borrow_mut().push(ws);
        self.show_workspace(id);
        self.toast(&format!("Workspace {name}"), 2.5);
    }

    /// The workspace showing `key`, or holding it in a hidden tab.
    fn workspace_containing(&self, key: &PaneKey) -> Option<u64> {
        if let Some(t) = self.tab_of(key) {
            return Some(t.workspace.get());
        }
        self.workspaces
            .borrow()
            .iter()
            .find(|ws| {
                ws.hidden_tabs
                    .iter()
                    .flat_map(|t| leaves(&t.root))
                    .any(|(h, pid)| PaneKey::new(h.unwrap_or(&ws.host), pid) == *key)
            })
            .map(|ws| ws.id)
    }

    fn rename_workspace(self: &Rc<Self>, id: u64, reopen_switcher: bool) {
        let Some(ws) = self.workspace(id) else { return };
        let Some(win) = self.win() else { return };
        dialogs::prompt(
            &win.window,
            "Rename Workspace",
            "",
            &[("", "Name", &ws.name)],
            "Rename",
            move |values| {
                with_app(|a| {
                    let name = values[0].trim().to_string();
                    if !name.is_empty()
                        && let Some(w) = a.workspaces.borrow_mut().iter_mut().find(|w| w.id == id)
                    {
                        w.name = name;
                    }
                    a.focus_changed();
                    a.schedule_save();
                    if reopen_switcher {
                        a.show_workspace_switcher();
                    }
                });
            },
        );
    }

    fn close_workspace(self: &Rc<Self>, id: u64) {
        let Some(ws) = self.workspace(id) else { return };
        let Some(win) = self.win() else { return };
        let count = if self.is_shown(id) {
            win.ordered_tabs().len()
        } else {
            ws.hidden_tabs.len()
        };
        let body = format!(
            "Its {count} {} and the programs running in them are closed.",
            if count == 1 { "tab" } else { "tabs" }
        );
        dialogs::confirm(
            &win.window,
            &format!("Close workspace “{}”?", ws.name),
            &body,
            "Close Workspace",
            true,
            move || {
                with_app(|a| a.close_workspace_now(id));
            },
        );
    }

    fn close_workspace_now(self: &Rc<Self>, id: u64) {
        let Some(ws) = self.workspace(id) else { return };
        let Some(win) = self.win() else { return };
        let shown = self.is_shown(id);
        let panes: Vec<PaneKey> = if shown {
            win.ordered_tabs().iter().flat_map(|t| t.panes()).collect()
        } else {
            ws.hidden_tabs
                .iter()
                .flat_map(|t| leaves(&t.root))
                .map(|(h, pid)| PaneKey::new(h.unwrap_or(&ws.host), pid))
                .collect()
        };
        for k in &panes {
            self.send_close_pane(k);
        }
        self.workspaces.borrow_mut().retain(|w| w.id != id);
        if shown {
            for t in win.ordered_tabs() {
                t.workspace.set(0);
            }
            let next = self
                .workspaces_by_recency()
                .into_iter()
                .find(|w| !w.hidden_tabs.is_empty())
                .map(|w| w.id);
            match next {
                Some(n) => self.show_workspace(n),
                None => {
                    let ws = self.make_workspace(LOCAL);
                    let nid = ws.id;
                    self.workspaces.borrow_mut().push(ws);
                    self.show_workspace(nid);
                }
            }
        }
        self.focus_changed();
        self.schedule_save();
    }

    pub fn move_tab_to_new_workspace(self: &Rc<Self>, tab: &Rc<Tab>) {
        let Some(win) = self.win() else { return };
        if win.ordered_tabs().len() <= 1 {
            win.window.error_bell();
            return;
        }
        let mut ws = self.make_workspace(&tab.host);
        ws.hidden_tabs = vec![tab.to_layout()];
        let name = ws.name.clone();
        self.workspaces.borrow_mut().push(ws);
        self.drop_tab_widgets(tab);
        win.remove_tab(tab);
        self.toast(&format!("Tab moved to workspace {name}"), 4.0);
        self.focus_changed();
        self.schedule_save();
    }

    /// The workspace menu (sidebar button, Window › Workspaces).
    pub fn workspace_menu(&self, with_actions: bool) -> gio::Menu {
        let menu = gio::Menu::new();
        let list = gio::Menu::new();
        let current = self.current_workspace();
        for ws in self.workspaces_by_recency() {
            let label = if Some(ws.id) == current {
                format!("✓ {}", ws.name)
            } else {
                ws.name.clone()
            };
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(
                Some("app.switch_to_workspace"),
                Some(&ws.id.to_variant()),
            );
            list.append_item(&item);
        }
        menu.append_section(None, &list);
        if with_actions {
            let actions = gio::Menu::new();
            actions.append(Some("New Workspace"), Some("app.new_workspace"));
            actions.append(Some("Switch Workspace…"), Some("app.switch_workspace"));
            if current.is_some() {
                actions.append(Some("Rename Workspace…"), Some("app.rename_workspace"));
                actions.append(Some("Close Workspace…"), Some("app.close_workspace"));
            }
            menu.append_section(None, &actions);
        }
        menu
    }

    fn show_workspace_switcher(self: &Rc<Self>) {
        let Some(win) = self.win() else { return };
        let current = self.current_workspace();
        let mut items = Vec::new();
        for ws in self.workspaces_by_recency() {
            let tabs = if Some(ws.id) == current {
                win.ordered_tabs().len()
            } else {
                ws.hidden_tabs.len()
            };
            let mut detail = String::new();
            if ws.is_remote() {
                detail.push_str(&format!(
                    "{} · {} · ",
                    ws.host,
                    self.remotes.phase_label(self, &ws.host)
                ));
            }
            detail.push_str(&format!("{tabs} {}", if tabs == 1 { "tab" } else { "tabs" }));
            if Some(ws.id) == current {
                detail.push_str(" · this window");
            } else {
                detail.push_str(&format!(" · {}", relative_time(ws.last_active)));
            }
            let id = ws.id;
            let mut item = Item::new(ws.name.clone(), detail, move || {
                with_app(|a| a.switch_workspace(id));
            });
            item.rename = Some(Rc::new(move || {
                with_app(|a| a.rename_workspace(id, true));
            }));
            items.push(item);
        }
        items.push(
            Item::new("New Workspace", "", || {
                with_app(|a| a.new_workspace(LOCAL));
            })
            .shortcut(self.shortcut_label("new_workspace")),
        );
        for name in self.ui().remote_names() {
            let phase = self.remotes.phase_label(self, &name);
            let n = name.clone();
            items.push(Item::new(format!("New Workspace on {name}"), phase, move || {
                with_app(|a| a.new_workspace(&n));
            }));
        }
        palette::show(
            &win,
            items,
            "Switch to workspace…",
            Some("↩ switch   Ctrl+R rename"),
            0,
            None,
        );
    }

    pub fn shortcut_label(&self, action: &str) -> String {
        self.gtk_app
            .accels_for_action(&format!("app.{action}"))
            .first()
            .map(|a| actions::label(a))
            .unwrap_or_default()
    }

    // MARK: panes and tabs

    /// Starts a shell (or `preset`, or `command`) on `host`; `None` on failure (shown).
    pub fn create_pane(
        &self,
        host: &str,
        inherit: Option<&PaneKey>,
        preset: Option<&str>,
        command: Option<Vec<String>>,
        fork: Option<&PaneKey>,
        hold: bool,
    ) -> Option<PaneKey> {
        let Some(core) = self.core(host) else {
            self.toast(
                &format!(
                    "{host} is not connected ({})",
                    self.remotes.phase_label(self, host)
                ),
                4.0,
            );
            return None;
        };
        let (cols, rows, cw, ch) = self.new_pane_size(inherit);
        let same = |k: Option<&PaneKey>| k.filter(|k| k.host == host).map(|k| k.id);
        let req = json!({"CreatePane": {
            "command": command,
            "cwd": null,
            "env": [],
            "size": {"cols": cols, "rows": rows, "cell_width": cw, "cell_height": ch},
            "agent_preset": preset,
            "inherit_cwd_from": same(inherit),
            "hold": hold,
            "fork_from": same(fork),
        }});
        let resp = core.request(&req);
        let Some(id) = resp.pointer("/PaneCreated/pane").and_then(Value::as_u64) else {
            let msg = resp
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Not connected to the Thurm daemon.")
                .to_string();
            if let Some(w) = self.win() {
                dialogs::inform(
                    &w.window,
                    if fork.is_some() {
                        "Could not fork the agent session"
                    } else {
                        "Could not start a new shell"
                    },
                    &msg,
                );
            }
            return None;
        };
        let key = PaneKey::new(host, id);
        if let Some(info) = core
            .request(&json!({"PaneInfo": {"pane": id}}))
            .get("PaneInfo")
            .and_then(|v| serde_json::from_value::<PaneInfo>(v.clone()).ok())
        {
            self.infos.borrow_mut().insert(key.clone(), info);
        }
        Some(key)
    }

    fn new_pane_size(&self, like: Option<&PaneKey>) -> (u16, u16, u16, u16) {
        if let Some(v) = like.and_then(|k| self.view(k)).filter(|v| v.area.width() > 0) {
            let (c, r) = v.grid_size();
            let (cw, ch) = v.cell_pixels();
            return (c, r, cw, ch);
        }
        let ui = self.ui();
        let f = self.fonts();
        (
            ui.cfg.window.columns.clamp(2, 1000),
            ui.cfg.window.rows.clamp(1, 1000),
            f.cell_w as u16,
            f.cell_h as u16,
        )
    }

    /// A new tab in the window, on the current workspace's host.
    pub fn new_tab(
        self: &Rc<Self>,
        preset: Option<&str>,
        command: Option<Vec<String>>,
        existing: Option<PaneKey>,
    ) -> Option<PaneKey> {
        let key = match existing {
            Some(k) => k,
            None => {
                let host = self.current_host();
                let inherit = self.focused_pane();
                self.create_pane(&host, inherit.as_ref(), preset, command, None, false)?
            }
        };
        self.place_new_tab(key.clone(), None, true);
        Some(key)
    }

    /// Shows `key` in a new tab: in the window when its workspace is on that host, else in a
    /// hidden workspace of the host (shown when `reveal`).
    pub fn place_new_tab(self: &Rc<Self>, key: PaneKey, handoff: Option<String>, reveal: bool) {
        let Some(win) = self.win() else { return };
        let ws_id = self.current_workspace();
        let ws_host = ws_id.and_then(|id| self.workspace(id)).map(|w| w.host);
        if ws_host.as_deref().is_none_or(|h| h == key.host) {
            let id = match ws_id {
                Some(id) => id,
                None => {
                    let ws = self.make_workspace(&key.host);
                    let id = ws.id;
                    self.workspaces.borrow_mut().push(ws);
                    id
                }
            };
            let tab = Tab::new(SplitNode::Leaf(key.clone()), key.clone(), id);
            *tab.handoff.borrow_mut() = handoff;
            win.add_tab(&tab, true);
            tab.sync_children();
            self.focus_pane(&key);
        } else {
            let target = self
                .workspaces_by_recency()
                .into_iter()
                .find(|w| w.host == key.host && Some(w.id) != ws_id)
                .map(|w| w.id)
                .unwrap_or_else(|| {
                    let ws = self.make_workspace(&key.host);
                    let id = ws.id;
                    self.workspaces.borrow_mut().push(ws);
                    id
                });
            let layout = TabLayout {
                title: None,
                root: SplitNode::Leaf(key.clone()).to_layout(),
                focused: key.id,
                zoomed: None,
                handoff,
            };
            let name = {
                let mut wss = self.workspaces.borrow_mut();
                let ws = wss.iter_mut().find(|w| w.id == target).unwrap();
                ws.hidden_tabs.push(layout);
                ws.hidden_selected = ws.hidden_tabs.len() - 1;
                ws.name.clone()
            };
            if reveal {
                self.show_workspace(target);
            } else {
                self.toast(
                    &format!(
                        "New tab in workspace {name} ({}) · {} to switch",
                        key.host,
                        self.shortcut_label("switch_workspace")
                    ),
                    5.0,
                );
            }
        }
        self.focus_changed();
        self.schedule_save();
    }

    pub fn split_focused(self: &Rc<Self>, dir: Direction, preset: Option<&str>, fork: bool) {
        let Some(tab) = self.current_tab() else {
            self.new_tab(preset, None, None);
            return;
        };
        let target = tab.focused.borrow().clone();
        let Some(key) = self.create_pane(
            &target.host,
            Some(&target),
            preset,
            None,
            fork.then_some(&target),
            false,
        ) else {
            return;
        };
        self.split_tab(&tab, &target, key, dir);
    }

    fn split_tab(self: &Rc<Self>, tab: &Rc<Tab>, target: &PaneKey, key: PaneKey, dir: Direction) {
        let target = if tab.contains(target) {
            target.clone()
        } else {
            tab.focused.borrow().clone()
        };
        *tab.zoomed.borrow_mut() = None;
        let tree = tab.tree.borrow().clone();
        *tab.tree.borrow_mut() = tree.split(&target, key.clone(), dir);
        tab.sync_children();
        self.focus_pane(&key);
        self.schedule_save();
    }

    /// Close Pane: asks when a program runs and `confirm_close` is on.
    pub fn user_close_pane(self: &Rc<Self>, key: &PaneKey) {
        let Some(tab) = self.tab_of(key) else { return };
        if tab.handoff.borrow().is_some() && tab.panes().len() == 1 {
            self.close_tab(&tab);
            return;
        }
        let busy = self
            .pane_info(key)
            .filter(|i| self.ui().cfg.window.confirm_close && model::has_running_process(i));
        let k = key.clone();
        let close = move || {
            with_app(|a| {
                a.send_close_pane(&k);
                a.remove_pane_from_ui(&k);
            });
        };
        match (busy, self.win()) {
            (Some(info), Some(w)) => {
                let name = info
                    .foreground
                    .map(|f| f.name)
                    .unwrap_or_else(|| "A process".into());
                dialogs::confirm(
                    &w.window,
                    "Close this pane?",
                    &format!("{name} is still running and will be terminated."),
                    "Close",
                    true,
                    close,
                );
            }
            _ => close(),
        }
    }

    /// `ClosePane`; an offline remote host gets it when it is back.
    pub fn send_close_pane(&self, key: &PaneKey) {
        match self.core(&key.host) {
            Some(c) => c.send(&json!({"ClosePane": {"pane": key.id}})),
            None if key.is_remote() => {
                let pid = self.pane_info(key).and_then(|i| i.pid).unwrap_or(0);
                self.remotes.queue_close(key, pid);
            }
            None => {}
        }
        self.infos.borrow_mut().remove(key);
    }

    /// Takes a pane out of its tab (closing the tab when it was the last pane).
    pub fn remove_pane_from_ui(self: &Rc<Self>, key: &PaneKey) {
        let removed = self.views.borrow_mut().remove(key);
        if let Some(v) = removed {
            v.detach();
            if v.root.parent().is_some() {
                v.root.unparent();
            }
        }
        for ws in self.workspaces.borrow_mut().iter_mut() {
            let host = ws.host.clone();
            retain_tabs(&mut ws.hidden_tabs, &|h, id| {
                PaneKey::new(h.unwrap_or(&host), id) != *key
            });
            ws.hidden_selected = ws.hidden_selected.min(ws.hidden_tabs.len().saturating_sub(1));
        }
        let Some(tab) = self.tab_of(key) else {
            self.focus_changed();
            return;
        };
        let was_focused = *tab.focused.borrow() == *key;
        let center = tab
            .frames()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, r)| r.center());
        if tab.zoomed.borrow().as_ref() == Some(key) {
            *tab.zoomed.borrow_mut() = None;
        }
        let tree = tab.tree.borrow().clone();
        match tree.remove(key) {
            Some(rest) => {
                *tab.tree.borrow_mut() = rest;
                tab.sync_children();
                if was_focused {
                    let frames: Vec<_> =
                        tab.frames().into_iter().filter(|(k, _)| k != key).collect();
                    let next = center
                        .and_then(|c| model::nearest(&frames, c))
                        .or_else(|| tab.panes().first().cloned());
                    if let Some(n) = next {
                        tab.set_focused(&n);
                        if self.current_tab().is_some_and(|t| Rc::ptr_eq(&t, &tab)) {
                            self.focus_pane(&n);
                        }
                    }
                }
            }
            None => {
                let quick = self.quick.borrow().clone();
                match quick {
                    Some(q) if Rc::ptr_eq(&q.tab, &tab) => q.closed(),
                    _ => {
                        if let Some(w) = self.win() {
                            w.remove_tab(&tab);
                            if w.ordered_tabs().is_empty() {
                                self.window_emptied();
                            }
                        }
                    }
                }
            }
        }
        self.focus_changed();
        self.schedule_save();
    }

    /// The window lost its last tab: the most recent hidden workspace comes up; else a fresh
    /// shell, or the app quits with `quit_after_last_window`.
    fn window_emptied(self: &Rc<Self>) {
        let cur = self.current_workspace();
        let next = self
            .workspaces_by_recency()
            .into_iter()
            .find(|w| Some(w.id) != cur && !w.hidden_tabs.is_empty())
            .map(|w| w.id);
        if let Some(id) = next {
            self.show_workspace(id);
            return;
        }
        if self.ui().cfg.window.quit_after_last_window {
            self.quit();
            return;
        }
        self.workspaces.borrow_mut().retain(|w| !w.hidden_tabs.is_empty());
        let ws = self.make_workspace(LOCAL);
        let id = ws.id;
        self.workspaces.borrow_mut().push(ws);
        self.show_workspace(id);
    }

    pub fn close_tab(self: &Rc<Self>, tab: &Rc<Tab>) {
        let page = tab.page.borrow().clone();
        if let (Some(p), Some(w)) = (page, self.win()) {
            w.tab_view.close_page(&p);
            return;
        }
        let quick = self.quick.borrow().clone();
        if let Some(q) = quick
            && Rc::ptr_eq(&q.tab, tab)
        {
            for k in tab.panes() {
                self.send_close_pane(&k);
            }
            q.closed();
        }
    }

    /// The tab view asks to close a page (its close button, Close Tab): confirm, then close
    /// every pane. Returns true when it answers the request itself.
    pub fn close_tab_page(self: &Rc<Self>, page: &adw::TabPage) -> bool {
        let Some(w) = self.win() else { return false };
        let Some(tab) = w.tab_for_page(page) else {
            return false;
        };
        if self.terminating.get() {
            return false;
        }
        let page = page.clone();
        let finish = {
            let tab = tab.clone();
            move |close: bool, remove_worktree: Option<bool>| {
                with_app(|a| {
                    let Some(w) = a.win() else { return };
                    if close {
                        let handoff = tab.handoff.borrow().clone();
                        if let (Some(remove), Some(h)) = (remove_worktree, handoff) {
                            remote::cleanup_handoff(a, &h, remove);
                        }
                        for k in tab.panes() {
                            a.send_close_pane(&k);
                        }
                        a.drop_tab_widgets(&tab);
                        w.tabs.borrow_mut().retain(|t| !Rc::ptr_eq(t, &tab));
                        tab.page.borrow_mut().take();
                    }
                    w.tab_view.close_page_finish(&page, close);
                    if close {
                        if w.ordered_tabs().is_empty() {
                            a.window_emptied();
                        }
                        a.focus_changed();
                        a.schedule_save();
                    }
                });
            }
        };
        let handoff = tab.handoff.borrow().clone();
        if let Some(info) = handoff.and_then(|h| self.remotes.handoff(&h)) {
            let host = info.get("host").and_then(Value::as_str).unwrap_or("").to_string();
            let branch = info.get("branch").and_then(Value::as_str).unwrap_or("").to_string();
            let worktree = info.get("worktree").and_then(Value::as_str).unwrap_or("").to_string();
            dialogs::ask(
                &w.window,
                &format!("Close the handoff on {host}?"),
                &format!(
                    "Closing the tab stops the agent. You can also remove its worktree, {branch} in {worktree}: committed work is fetched first, and the branch stays on {host} unless your default branch contains it."
                ),
                &[
                    dialogs::button("remove", "Close and Remove Worktree", dialogs::Style::Destructive),
                    dialogs::button("keep", "Close, Keep Worktree", dialogs::Style::Default),
                    dialogs::button("cancel", "Cancel", dialogs::Style::Default),
                ],
                move |r| match r.as_str() {
                    "remove" => finish(true, Some(true)),
                    "keep" => finish(true, Some(false)),
                    _ => finish(false, None),
                },
            );
            return true;
        }
        let busy: Vec<String> = tab
            .panes()
            .iter()
            .filter_map(|k| self.pane_info(k))
            .filter(model::has_running_process)
            .filter_map(|i| i.foreground.map(|f| f.name))
            .collect();
        if self.ui().cfg.window.confirm_close && !busy.is_empty() {
            dialogs::ask(
                &w.window,
                "Close this tab?",
                &format!(
                    "Processes are still running: {}. Closing the tab terminates them.",
                    busy.join(", ")
                ),
                &[
                    dialogs::button("close", "Close", dialogs::Style::Destructive),
                    dialogs::button("cancel", "Cancel", dialogs::Style::Default),
                ],
                move |r| finish(r == "close", None),
            );
        } else {
            finish(true, None);
        }
        true
    }

    pub fn focus_pane(&self, key: &PaneKey) {
        let Some(tab) = self.tab_of(key) else { return };
        let quick = self.quick.borrow().clone();
        match quick {
            Some(q) if Rc::ptr_eq(&q.tab, &tab) => q.show(),
            _ => {
                if let Some(w) = self.win() {
                    w.select_tab(&tab);
                }
            }
        }
        tab.set_focused(key);
        if let Some(v) = self.view(key) {
            v.grab_focus();
        }
    }

    /// A view took the keyboard focus.
    pub fn pane_focused(&self, key: &PaneKey) {
        if let Some(tab) = self.tab_of(key) {
            let changed = *tab.focused.borrow() != *key;
            tab.set_focused(key);
            if changed {
                self.schedule_save();
            }
        }
        self.focus_changed();
    }

    fn move_focus(&self, dir: Direction) {
        let Some(tab) = self.current_tab() else { return };
        if tab.zoomed.borrow().is_some() {
            return;
        }
        let cur = tab.focused.borrow().clone();
        if let Some(n) = model::neighbor(&tab.frames(), &cur, dir) {
            self.focus_pane(&n);
        }
    }

    fn resize_focused(&self, axis: Axis, delta: f64) {
        let Some(tab) = self.current_tab() else { return };
        let cur = tab.focused.borrow().clone();
        if tab.tree.borrow_mut().resize(&cur, axis, delta) {
            tab.relayout();
            self.schedule_save();
        }
    }

    fn toggle_zoom(&self) {
        let Some(tab) = self.current_tab() else { return };
        if tab.panes().len() < 2 && tab.zoomed.borrow().is_none() {
            return;
        }
        let z = if tab.zoomed.borrow().is_some() {
            None
        } else {
            Some(tab.focused.borrow().clone())
        };
        *tab.zoomed.borrow_mut() = z;
        tab.relayout();
        self.schedule_save();
    }

    /// Tabs in the order Alt+1…9 count them: the sidebar's (grouped by repository) when it
    /// shows the tabs, else the tab bar's.
    pub fn display_order(&self) -> Vec<Rc<Tab>> {
        let Some(w) = self.win() else { return Vec::new() };
        let tabs = w.ordered_tabs();
        if !self.ui().sidebar_tabs() {
            return tabs;
        }
        self.grouped_tabs(&tabs).into_iter().flat_map(|(_, t)| t).collect()
    }

    fn select_tab_number(&self, n: usize) {
        let tabs = self.display_order();
        if tabs.is_empty() {
            return;
        }
        let i = if n >= 9 { tabs.len() - 1 } else { n - 1 };
        if let Some(t) = tabs.get(i) {
            self.select_tab(t);
        }
    }

    pub fn select_tab(&self, tab: &Rc<Tab>) {
        if let Some(w) = self.win() {
            w.select_tab(tab);
            let k = tab.focused.borrow().clone();
            self.focus_pane(&k);
        }
    }

    fn cycle_tab(&self, forward: bool) {
        let Some(w) = self.win() else { return };
        let tabs = w.ordered_tabs();
        let Some(cur) = w.selected_tab() else { return };
        let n = tabs.len();
        let i = tabs.iter().position(|t| Rc::ptr_eq(t, &cur)).unwrap_or(0);
        let next = if forward { (i + 1) % n } else { (i + n - 1) % n };
        self.select_tab(&tabs[next]);
    }

    pub fn tab_selected(&self) {
        if let Some(t) = self.current_tab() {
            let k = t.focused.borrow().clone();
            if let Some(v) = self.view(&k) {
                v.grab_focus();
            }
        }
        self.focus_changed();
        self.schedule_save();
    }

    pub fn tabs_reordered(&self) {
        self.focus_changed();
        self.schedule_save();
    }

    pub fn tab_index(&self, tab: &Rc<Tab>) -> Option<usize> {
        self.win()?
            .ordered_tabs()
            .iter()
            .position(|t| Rc::ptr_eq(t, tab))
    }

    /// Sidebar drag: moves tab `from` before (or after) `target`.
    pub fn move_tab(&self, from: usize, target: &Rc<Tab>, after: bool) {
        let Some(w) = self.win() else { return };
        let tabs = w.ordered_tabs();
        let (Some(moving), Some(to)) = (tabs.get(from).cloned(), self.tab_index(target)) else {
            return;
        };
        let mut pos = if after { to + 1 } else { to };
        if from < pos {
            pos -= 1;
        }
        if let Some(page) = moving.page.borrow().as_ref() {
            w.tab_view
                .reorder_page(page, pos.min(tabs.len() - 1) as i32);
        }
        self.select_tab(&moving);
        self.schedule_save();
    }

    pub fn set_menu_tab(&self, tab: &Rc<Tab>) {
        *self.menu_tab.borrow_mut() = Some(Rc::downgrade(tab));
    }

    pub fn toggle_group(&self, name: &str) {
        {
            let mut c = self.collapsed_groups.borrow_mut();
            match c.iter().position(|n| n == name) {
                Some(i) => {
                    c.remove(i);
                }
                None => c.push(name.to_string()),
            }
            integrations::save_state_list("collapsed_groups", &c);
        }
        self.refresh_sidebar();
    }

    // MARK: agents

    /// Every agent pane: the window's tabs, then the quick terminal, then hidden workspaces
    /// (with the workspace's name).
    pub fn agent_panes(&self) -> Vec<(PaneKey, Option<String>)> {
        let infos = self.infos.borrow();
        let mut out = Vec::new();
        let has_agent = |k: &PaneKey| infos.get(k).is_some_and(|i| i.agent.is_some());
        for t in self.all_tabs() {
            for k in t.panes() {
                if has_agent(&k) {
                    out.push((k, None));
                }
            }
        }
        for ws in self.workspaces.borrow().iter() {
            for t in &ws.hidden_tabs {
                for (h, id) in leaves(&t.root) {
                    let k = PaneKey::new(h.unwrap_or(&ws.host), id);
                    if has_agent(&k) {
                        out.push((k, Some(ws.name.clone())));
                    }
                }
            }
        }
        out
    }

    pub fn focus_agent(self: &Rc<Self>, key: &PaneKey) {
        if self.tab_of(key).is_none() {
            let ws = self
                .workspaces
                .borrow()
                .iter()
                .find(|w| {
                    w.hidden_tabs.iter().any(|t| {
                        leaves(&t.root)
                            .iter()
                            .any(|(h, id)| PaneKey::new(h.unwrap_or(&w.host), *id) == *key)
                    })
                })
                .map(|w| w.id);
            if let Some(id) = ws {
                self.show_workspace(id);
            }
        }
        self.focus_pane(key);
        if let Some(w) = self.win() {
            w.window.present();
        }
    }

    fn show_agent_picker(self: &Rc<Self>) {
        let Some(win) = self.win() else { return };
        let here = self.focused_pane();
        let infos = self.infos.borrow().clone();
        let mut agents = self.agent_panes();
        agents.sort_by_key(|(k, _)| {
            std::cmp::Reverse(
                infos
                    .get(k)
                    .and_then(|i| i.agent.as_ref())
                    .map_or(0, |a| model::urgency(a.status)),
            )
        });
        let mut items = Vec::new();
        let any = !agents.is_empty();
        for (k, hidden) in agents {
            let Some(info) = infos.get(&k) else { continue };
            let Some(agent) = info.agent.as_ref() else { continue };
            let mut parts: Vec<String> = Vec::new();
            if k.is_remote() {
                parts.push(k.host.clone());
            }
            parts.push(agent.name.clone());
            parts.push(self.place(&k, info));
            if let Some(h) = hidden {
                parts.push(h);
            }
            parts.push(if Some(&k) == here.as_ref() {
                "this pane".into()
            } else {
                model::status_detail(info)
            });
            parts.retain(|p| !p.is_empty());
            let key = k.clone();
            items.push(Item::new(model::agent_title(info), parts.join(" · "), move || {
                with_app(|a| a.focus_agent(&key));
            }));
        }
        let host = self.current_host();
        for p in self.presets(&host) {
            let name = p.name.clone();
            let title = if host == LOCAL {
                format!("Launch {name}")
            } else {
                format!("Launch {name} on {host}")
            };
            items.push(Item::new(title, p.command.join(" "), move || {
                with_app(|a| {
                    a.new_tab(Some(&name), None, None);
                });
            }));
        }
        palette::show(
            &win,
            items,
            if any {
                "Switch to agent…"
            } else {
                "No agents running. Start one…"
            },
            Some("↩ switch"),
            0,
            None,
        );
    }

    pub fn presets(&self, host: &str) -> Vec<thurm_proto::AgentPreset> {
        if host == LOCAL {
            self.ui().presets.clone()
        } else {
            self.remotes.presets(host)
        }
    }

    /// Where an agent pane works: its repository, else its directory.
    fn place(&self, k: &PaneKey, info: &PaneInfo) -> String {
        if let Some(g) = &info.git {
            return g.root.rsplit('/').next().unwrap_or(&g.root).to_string();
        }
        match &info.cwd {
            Some(c) if k.is_remote() => c.clone(),
            Some(c) => model::abbreviate_path(c, home().as_deref()),
            None => String::new(),
        }
    }

    // MARK: palette

    fn show_command_palette(self: &Rc<Self>) {
        let Some(win) = self.win() else { return };
        let mut items = Vec::new();
        for m in actions::Menu::PALETTE_ORDER {
            for def in ACTIONS.iter().filter(|d| d.menu == m) {
                if !self.action_visible(def.name) || actions::hidden_from_menu(def.name) {
                    continue;
                }
                let name = def.name;
                let label = self.action_label(name);
                items.push(
                    Item::new(label.trim_start_matches("✓ "), m.title(), move || {
                        with_app(|a| a.run_action(name));
                    })
                    .shortcut(self.shortcut_label(name)),
                );
            }
        }
        for (label, action) in integrations::palette_items() {
            items.push(Item::new(format!("Integrations: {label}"), "Thurm", move || {
                action();
            }));
        }
        if let Some(k) = self.focused_pane()
            && let Some(agent) = self
                .pane_info(&k)
                .and_then(|i| i.agent)
                .filter(|a| a.session_id.is_some())
        {
            items.push(Item::new(format!("Fork {} Session", agent.name), "in a split", || {
                with_app(|a| a.split_focused(Direction::Right, None, true));
            }));
        }
        items.extend(remote::palette_items(self));
        let host = self.current_host();
        for p in self.presets(&host) {
            let detail = p.command.join(" ");
            let n1 = p.name.clone();
            items.push(Item::new(format!("Launch {}", p.name), detail.clone(), move || {
                with_app(|a| {
                    a.new_tab(Some(&n1), None, None);
                });
            }));
            let n2 = p.name.clone();
            items.push(Item::new(format!("Launch {} in Split", p.name), detail, move || {
                with_app(|a| a.split_focused(Direction::Right, Some(&n2), false));
            }));
        }
        palette::show(&win, items, "Type a command…", None, 0, None);
    }

    // MARK: titles, sidebar, badges

    /// After focus, pane info or tab changes: titles, sidebar, badge, action states, locks.
    pub fn focus_changed(&self) {
        self.update_titles();
        self.refresh_sidebar();
        notify::update_badge(self);
        self.update_action_states();
        self.update_locks();
    }

    fn update_titles(&self) {
        let Some(w) = self.win() else { return };
        let infos = self.infos.borrow();
        let order = self.display_order();
        let count = order.len();
        for (i, t) in order.iter().enumerate() {
            let focused = t.focused.borrow().clone();
            let base = t
                .title
                .borrow()
                .clone()
                .unwrap_or_else(|| model::display_title(infos.get(&focused)));
            let status = model::tab_status(t.panes().iter().filter_map(|k| infos.get(k)));
            let host = if t.host == LOCAL {
                String::new()
            } else {
                format!("{} · ", t.host)
            };
            let title = format!("{}{host}{base}", model::status_prefix(status));
            if let Some(p) = t.page.borrow().as_ref() {
                p.set_title(&format!("{host}{base}"));
                let icon = status.map(|s| {
                    gio::ThemedIcon::new(match s {
                        AgentStatus::NeedsInput => "dialog-warning-symbolic",
                        AgentStatus::Working => "content-loading-symbolic",
                        AgentStatus::Done => "object-select-symbolic",
                        AgentStatus::Idle => "media-playback-pause-symbolic",
                    })
                    .upcast::<gio::Icon>()
                });
                p.set_indicator_icon(icon.as_ref());
                p.set_needs_attention(status == Some(AgentStatus::NeedsInput));
                let tip = t.panes().iter().filter_map(|k| infos.get(k)).find_map(|i| {
                    let a = i.agent.as_ref()?;
                    let detail = match (a.status, a.turn_ms) {
                        (AgentStatus::Done, Some(ms)) => {
                            format!("finished in {}", model::format_duration(ms))
                        }
                        _ => a
                            .message
                            .clone()
                            .unwrap_or_else(|| model::status_label(a.status).into()),
                    };
                    Some(format!("{}: {detail}", a.name))
                });
                let shortcut = tab_shortcut(i, count)
                    .map(|n| format!(" (Alt+{n})"))
                    .unwrap_or_default();
                p.set_tooltip(&glib::markup_escape_text(
                    &(tip.unwrap_or_else(|| base.clone()) + &shortcut),
                ));
            }
            if w.selected_tab().is_some_and(|s| Rc::ptr_eq(&s, t)) {
                let ws = self
                    .current_workspace()
                    .and_then(|id| self.workspace(id))
                    .map(|w| w.name)
                    .unwrap_or_default();
                w.set_title(&title, &ws);
            }
        }
    }

    pub fn refresh_sidebar(&self) {
        if self.sidebar_refresh.replace(true) {
            return;
        }
        glib::idle_add_local_once(|| {
            with_app(|a| {
                a.sidebar_refresh.set(false);
                a.refresh_sidebar_now();
            });
        });
    }

    /// Tabs grouped by the focused pane's repository ("Terminals" for the rest, last).
    fn grouped_tabs(&self, tabs: &[Rc<Tab>]) -> Vec<(String, Vec<Rc<Tab>>)> {
        let infos = self.infos.borrow();
        let mut groups: Vec<(String, String, Vec<Rc<Tab>>)> = Vec::new();
        let mut rest = Vec::new();
        for t in tabs {
            let focused = t.focused.borrow().clone();
            let handoff = t.handoff.borrow().as_ref().and_then(|h| self.remotes.handoff(h));
            let key_name = match handoff {
                Some(h) => h.get("repo").and_then(Value::as_str).map(|r| {
                    (r.to_string(), r.rsplit('/').next().unwrap_or(r).to_string())
                }),
                None => infos.get(&focused).and_then(|i| i.git.as_ref()).map(|g| {
                    let base = g.root.rsplit('/').next().unwrap_or(&g.root).to_string();
                    if focused.is_remote() {
                        (
                            format!("{}:{}", focused.host, g.root),
                            format!("{} · {base}", focused.host),
                        )
                    } else {
                        (g.root.clone(), base)
                    }
                }),
            };
            match key_name {
                Some((key, name)) => match groups.iter_mut().find(|(k, _, _)| *k == key) {
                    Some(g) => g.2.push(t.clone()),
                    None => groups.push((key, name, vec![t.clone()])),
                },
                None => rest.push(t.clone()),
            }
        }
        let mut out: Vec<(String, Vec<Rc<Tab>>)> =
            groups.into_iter().map(|(_, n, t)| (n, t)).collect();
        if !rest.is_empty() {
            out.push(("Terminals".into(), rest));
        }
        out
    }

    fn refresh_sidebar_now(&self) {
        let Some(w) = self.win() else { return };
        let ws_name = self
            .current_workspace()
            .and_then(|id| self.workspace(id))
            .map(|w| w.name)
            .unwrap_or_else(|| "Workspace".into());
        w.sidebar.set_workspace_name(&ws_name);
        if !self.ui().sidebar_tabs() {
            return;
        }
        let tabs = w.ordered_tabs();
        let grouped = self.grouped_tabs(&tabs);
        let multiple = grouped.len() > 1;
        let selected = w.selected_tab();
        let count = tabs.len();
        let home = home();
        let mut index = 0;
        let mut groups = Vec::new();
        let mut group_tabs = Vec::new();
        {
            let infos = self.infos.borrow();
            for (name, ts) in &grouped {
                let mut rows = Vec::new();
                for t in ts {
                    let focused = t.focused.borrow().clone();
                    let info = infos.get(&focused);
                    let title = t
                        .title
                        .borrow()
                        .clone()
                        .unwrap_or_else(|| model::display_title(info));
                    let dir = info.and_then(|i| i.cwd.clone()).map(|c| {
                        if focused.is_remote() {
                            format!("{}:{c}", focused.host)
                        } else {
                            model::abbreviate_path(&c, home.as_deref())
                        }
                    });
                    let mut subtitle = String::new();
                    let handoff = t.handoff.borrow().as_ref().and_then(|h| self.remotes.handoff(h));
                    if let Some(h) = handoff {
                        let host = h.get("host").and_then(Value::as_str).unwrap_or("");
                        let branch = h.get("branch").and_then(Value::as_str).unwrap_or("");
                        subtitle = glib::markup_escape_text(&format!("{host} · {branch}")).to_string();
                        let id = h.get("id").and_then(Value::as_str).unwrap_or("");
                        if self.remotes.handoff_error(id).is_some() {
                            subtitle.push_str(" · fetch failed");
                        }
                    } else if let Some(g) = info.and_then(|i| i.git.as_ref()) {
                        let branch = if focused.is_remote() {
                            format!("{} · {}", focused.host, g.branch)
                        } else {
                            g.branch.clone()
                        };
                        subtitle = glib::markup_escape_text(&branch).to_string();
                        if g.added > 0 {
                            subtitle.push_str(&format!(
                                "  <span foreground=\"#30d158\">+{}</span>",
                                g.added
                            ));
                        }
                        if g.removed > 0 {
                            subtitle.push_str(&format!(
                                " <span foreground=\"#ff453a\">−{}</span>",
                                g.removed
                            ));
                        }
                    } else if let Some(d) = &dir
                        && *d != title
                        && !title.ends_with(d.as_str())
                    {
                        subtitle = glib::markup_escape_text(d).to_string();
                    }
                    let status = model::tab_status(t.panes().iter().filter_map(|k| infos.get(k)));
                    rows.push(TabRow {
                        tooltip: format!("{title}\n{}", dir.clone().unwrap_or_default()),
                        title,
                        subtitle,
                        status,
                        shortcut: tab_shortcut(index, count)
                            .map(|n| format!("Alt+{n}"))
                            .unwrap_or_default(),
                        selected: selected.as_ref().is_some_and(|s| Rc::ptr_eq(s, t)),
                    });
                    index += 1;
                }
                groups.push(Group {
                    name: multiple.then(|| name.clone()),
                    rows,
                });
                group_tabs.push(ts.clone());
            }
        }
        let here = self.focused_pane();
        let mut agents = Vec::new();
        for (k, hidden) in self.agent_panes() {
            let infos = self.infos.borrow();
            let Some(info) = infos.get(&k) else { continue };
            let mut parts = Vec::new();
            if k.is_remote() {
                parts.push(k.host.clone());
            }
            parts.push(self.place(&k, info));
            if let Some(h) = hidden {
                parts.push(h);
            }
            parts.push(model::status_detail(info));
            parts.retain(|p| !p.is_empty());
            agents.push(AgentRow {
                title: model::agent_title(info),
                subtitle: parts.join(" · "),
                status: info.agent.as_ref().map(|a| a.status),
                selected: Some(&k) == here.as_ref() && w.window.is_active(),
                key: k.clone(),
            });
        }
        let collapsed = self.collapsed_groups.borrow().clone();
        w.sidebar.update(groups, group_tabs, &collapsed, agents);
    }

    /// The lock badge: a password prompt in the window's focused pane.
    fn update_locks(&self) {
        let on = {
            let ui = self.ui();
            ui.cfg.security.auto_secure_input && ui.cfg.security.secure_input_indicator
        };
        let focused = self.focused_pane();
        let infos = self.infos.borrow();
        for (k, v) in self.views.borrow().iter() {
            let lock = on
                && Some(k) == focused.as_ref()
                && infos.get(k).is_some_and(|i| i.password_input);
            v.set_lock(lock);
        }
    }

    pub fn is_pane_focused(&self, key: &PaneKey) -> bool {
        self.win().is_some_and(|w| w.window.is_active())
            && self.focused_pane().as_ref() == Some(key)
    }

    pub fn seen_waiting(&self) -> std::cell::RefMut<'_, HashSet<PaneKey>> {
        self.seen_waiting.borrow_mut()
    }

    pub fn window_activity_changed(&self) {
        for v in self.views() {
            v.window_activity_changed();
        }
        self.focus_changed();
    }

    pub fn sidebar_toggled(&self) {
        self.refresh_sidebar();
    }

    pub fn sidebar_width_changed(&self, width: f64) {
        if width < 180.0 || (width - self.ui().cfg.window.sidebar_width).abs() < 1.0 {
            return;
        }
        if let Some(c) = self.core(LOCAL) {
            c.request(&json!({"SetSetting": {"key": "window.sidebar_width", "value": format!("{width:.1}")}}));
        }
    }

    pub fn agent_rows_changed(&self, rows: u32) {
        if rows == self.ui().cfg.window.sidebar_agent_rows {
            return;
        }
        if let Some(c) = self.core(LOCAL) {
            c.request(&json!({"SetSetting": {"key": "window.sidebar_agent_rows", "value": rows.to_string()}}));
        }
    }

    // MARK: daemon events

    pub fn frame_arrived(&self, key: &PaneKey, epoch: u64) {
        if self.epochs.borrow().get(&key.host) != Some(&epoch) {
            return;
        }
        if let Some(v) = self.view(key) {
            v.frame_arrived();
        }
    }

    pub fn handle_event(self: &Rc<Self>, host: &str, epoch: u64, event: Value) {
        if self.terminating.get() || self.epochs.borrow().get(host) != Some(&epoch) {
            return;
        }
        let (name, payload) = match &event {
            Value::String(s) => (s.as_str(), Value::Null),
            Value::Object(m) if m.len() == 1 => {
                let (k, v) = m.iter().next().unwrap();
                (k.as_str(), v.clone())
            }
            _ => return,
        };
        let key = payload
            .get("pane")
            .and_then(Value::as_u64)
            .map(|id| PaneKey::new(host, id));
        match name {
            "PaneInfo" => {
                if let Ok(info) = serde_json::from_value::<PaneInfo>(payload) {
                    self.pane_info_updated(PaneKey::new(host, info.id), info);
                }
            }
            "PaneExited" | "PaneClosed" => {
                if let Some(k) = key {
                    notify::withdraw_permission(self, &k);
                    self.infos.borrow_mut().remove(&k);
                    self.remove_pane_from_ui(&k);
                }
            }
            "Bell" => {
                if let Some(k) = key {
                    if let Some(v) = self.view(&k) {
                        v.flash();
                    }
                    if !self.is_pane_focused(&k)
                        && let Some(w) = self.win()
                    {
                        w.window.error_bell();
                    }
                }
            }
            "Notify" => {
                if let Some(k) = key {
                    notify::post(self, &k, &payload);
                }
            }
            "ClipboardStore" => {
                let user = payload.get("user").and_then(Value::as_bool) == Some(true);
                if let Some(text) = payload.get("text").and_then(Value::as_str)
                    && (user || host == LOCAL || remote::clipboard_write_allowed(host))
                {
                    self.display().clipboard().set_text(text);
                }
            }
            "ClipboardRequest" => {
                if let Some(k) = key {
                    remote::clipboard_request(self, &k);
                }
            }
            "Ui" => self.handle_ui(host, &payload),
            "ConfigReloaded" => {
                if host == LOCAL {
                    self.reload_config(false);
                }
            }
            "Disconnected" => {
                if host == LOCAL {
                    self.disconnected();
                } else {
                    remote::connection_lost(self, host);
                }
            }
            _ => {}
        }
    }

    pub fn pane_info_updated(self: &Rc<Self>, key: PaneKey, info: PaneInfo) {
        let old = self.infos.borrow_mut().insert(key.clone(), info.clone());
        let old_agent = old.as_ref().and_then(|o| o.agent.clone());
        let status = info.agent.as_ref().map(|a| a.status);
        if matches!(status, Some(AgentStatus::Done) | Some(AgentStatus::NeedsInput))
            && old_agent.as_ref().map(|a| a.status) != status
        {
            remote::agent_settled(self, &key);
        }
        let was_waiting = old_agent
            .as_ref()
            .is_some_and(|a| a.status == AgentStatus::NeedsInput);
        let waiting = status == Some(AgentStatus::NeedsInput);
        let old_message = old_agent.as_ref().and_then(|a| a.message.clone());
        let new_message = info.agent.as_ref().and_then(|a| a.message.clone());
        if !waiting || !was_waiting || old_message != new_message {
            self.seen_waiting.borrow_mut().remove(&key);
        }
        let old_permission = old_agent.as_ref().and_then(|a| a.permission);
        if old_permission.is_some() && old_permission != info.agent.as_ref().and_then(|a| a.permission)
        {
            notify::withdraw_permission(self, &key);
        }
        if waiting
            && !was_waiting
            && !self.is_pane_focused(&key)
            && let Some(w) = self.win()
        {
            w.window.error_bell();
        }
        if let Some(v) = self.view(&key) {
            let lock = self.ui().cfg.security.secure_input_indicator
                && info.password_input
                && self.focused_pane().as_ref() == Some(&key);
            view::info_updated(&v, &info, lock);
        }
        // A pane started elsewhere (`thurm new`) with no UI command: it gets a tab shortly
        // unless a NewTab or Split places it first.
        if info.alive && old.is_none() && self.session_ready.get() && !self.is_placed(&key) {
            let k = key.clone();
            glib::timeout_add_local_once(Duration::from_millis(400), move || {
                with_app(|a| {
                    if a.infos.borrow().contains_key(&k) && !a.is_placed(&k) {
                        a.place_new_tab(k.clone(), None, true);
                    }
                });
            });
        }
        self.focus_changed();
    }

    fn is_placed(&self, key: &PaneKey) -> bool {
        self.tab_of(key).is_some()
            || self.workspaces.borrow().iter().any(|w| {
                w.hidden_tabs.iter().any(|t| {
                    leaves(&t.root)
                        .iter()
                        .any(|(h, id)| PaneKey::new(h.unwrap_or(&w.host), *id) == *key)
                })
            })
            || self.quick_layout.borrow().as_ref().is_some_and(|t| {
                leaves(&t.root)
                    .iter()
                    .any(|(h, id)| PaneKey::new(h.unwrap_or(LOCAL), *id) == *key)
            })
    }

    fn fetch_info(&self, key: &PaneKey) {
        if self.infos.borrow().contains_key(key) {
            return;
        }
        if let Some(c) = self.core(&key.host)
            && let Some(info) = c
                .request(&json!({"PaneInfo": {"pane": key.id}}))
                .get("PaneInfo")
                .and_then(|v| serde_json::from_value::<PaneInfo>(v.clone()).ok())
        {
            self.infos.borrow_mut().insert(key.clone(), info);
        }
    }

    fn handle_ui(self: &Rc<Self>, host: &str, ui: &Value) {
        let Some((name, p)) = ui.as_object().and_then(|m| m.iter().next()) else {
            return;
        };
        let key = p
            .get("pane")
            .and_then(Value::as_u64)
            .map(|id| PaneKey::new(host, id));
        match name.as_str() {
            "NewTab" => {
                let Some(k) = key else { return };
                if self.tab_of(&k).is_some() {
                    self.focus_pane(&k);
                    return;
                }
                self.fetch_info(&k);
                let handoff = self.remotes.handoff_for_pane(&k);
                if p.get("new_window").and_then(Value::as_bool) == Some(true) {
                    // Its own new workspace, in the background.
                    let mut ws = self.make_workspace(&k.host);
                    ws.hidden_tabs.push(TabLayout {
                        title: None,
                        root: SplitNode::Leaf(k.clone()).to_layout(),
                        focused: k.id,
                        zoomed: None,
                        handoff,
                    });
                    let name = ws.name.clone();
                    self.workspaces.borrow_mut().push(ws);
                    self.toast(
                        &format!(
                            "New tab in workspace {name} · {} to switch",
                            self.shortcut_label("switch_workspace")
                        ),
                        5.0,
                    );
                    self.focus_changed();
                    self.schedule_save();
                } else {
                    self.place_new_tab(k, handoff, true);
                }
                if let Some(w) = self.win() {
                    w.window.present();
                }
            }
            "Split" => {
                let Some(k) = key else { return };
                if self.tab_of(&k).is_some() {
                    self.focus_pane(&k);
                    return;
                }
                self.fetch_info(&k);
                let dir =
                    Direction::parse(p.get("dir").and_then(Value::as_str).unwrap_or("right"));
                let target = p
                    .get("target")
                    .and_then(Value::as_u64)
                    .map(|id| PaneKey::new(host, id));
                let tab = target
                    .as_ref()
                    .and_then(|t| self.tab_of(t))
                    .or_else(|| self.current_tab().filter(|t| t.host == host));
                match tab {
                    Some(t) => {
                        let tgt = target.unwrap_or_else(|| t.focused.borrow().clone());
                        self.split_tab(&t, &tgt, k, dir);
                    }
                    None => self.place_new_tab(k, None, true),
                }
            }
            "Focus" => {
                if let Some(k) = key {
                    self.focus_agent(&k);
                }
            }
            "SetTabTitle" => {
                if let Some(t) = key.and_then(|k| self.tab_of(&k)) {
                    *t.title.borrow_mut() = p
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    self.focus_changed();
                    self.schedule_save();
                }
            }
            "RenameWorkspace" => {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
                if !name.is_empty()
                    && let Some(id) = key.as_ref().and_then(|k| self.workspace_containing(k))
                {
                    if let Some(w) = self.workspaces.borrow_mut().iter_mut().find(|w| w.id == id) {
                        w.name = name;
                    }
                    self.focus_changed();
                    self.schedule_save();
                }
            }
            "Scroll" => {
                if let (Some(k), Some(scroll)) = (key, p.get("scroll"))
                    && let Some(c) = self.core(&k.host)
                {
                    c.send(&json!({"Scroll": {"pane": k.id, "scroll": scroll}}));
                    if let Some(v) = self.view(&k) {
                        v.area.queue_draw();
                    }
                }
            }
            _ => {}
        }
    }

    // MARK: connection

    pub fn next_epoch(&self, host: &str) -> u64 {
        let mut e = self.epochs.borrow_mut();
        let n = e.get(host).copied().unwrap_or(0) + 1;
        e.insert(host.to_string(), n);
        n
    }

    pub fn set_core(&self, host: &str, core: Option<Core>) {
        match core {
            Some(c) => {
                self.cores.borrow_mut().insert(host.to_string(), Rc::new(c));
            }
            None => {
                self.cores.borrow_mut().remove(host);
                self.next_epoch(host);
            }
        }
    }

    fn disconnected(self: &Rc<Self>) {
        self.cores.borrow_mut().remove(LOCAL);
        if self.reconnecting.replace(true) {
            return;
        }
        self.toast("Daemon connection lost, reconnecting…", 3.0);
        self.reconnect_attempt.set(0);
        self.attempt_reconnect();
    }

    fn attempt_reconnect(self: &Rc<Self>) {
        let epoch = self.next_epoch(LOCAL);
        match UPGRADE_TRIED.with(|u| connect_local(epoch, u)) {
            Ok(core) => {
                core.send(&json!({"SetAppearance": {"dark": system_is_dark()}}));
                self.cores.borrow_mut().insert(LOCAL.into(), Rc::new(core));
                self.reconnecting.set(false);
                self.resync();
            }
            Err(ConnectError::Failed(e) | ConnectError::OldDaemon(e)) => {
                log::debug!("reconnect: {e}");
                let n = self.reconnect_attempt.get();
                self.reconnect_attempt.set(n + 1);
                let delay = (0.25 * 2f64.powi(n as i32 + 1)).min(5.0);
                glib::timeout_add_local_once(Duration::from_secs_f64(delay), || {
                    with_app(|a| a.attempt_reconnect());
                });
            }
        }
    }

    fn resync(self: &Rc<Self>) {
        let Some(core) = self.core(LOCAL) else { return };
        let panes = core.request(&json!("ListPanes"));
        let mut listed = HashSet::new();
        {
            let mut infos = self.infos.borrow_mut();
            infos.retain(|k, _| k.is_remote());
            for p in panes.get("Panes").and_then(Value::as_array).into_iter().flatten() {
                if let Ok(info) = serde_json::from_value::<PaneInfo>(p.clone()) {
                    listed.insert(info.id);
                    infos.insert(PaneKey::local(info.id), info);
                }
            }
        }
        let shown: Vec<PaneKey> = self
            .views
            .borrow()
            .keys()
            .filter(|k| !k.is_remote())
            .cloned()
            .collect();
        if !shown.is_empty() && !shown.iter().any(|k| listed.contains(&k.id)) {
            // A restarted daemon numbers its restored panes anew: start over from its layout.
            if let Some(w) = self.win() {
                for t in w.ordered_tabs() {
                    self.drop_tab_widgets(&t);
                    w.remove_tab(&t);
                }
            }
            self.session_ready.set(false);
            self.restore_session();
            return;
        }
        for k in shown {
            if listed.contains(&k.id) {
                if let Some(v) = self.view(&k) {
                    v.resubscribe();
                }
            } else {
                self.remove_pane_from_ui(&k);
            }
        }
        self.toast("Reconnected", 2.5);
        self.save_now(true);
        self.focus_changed();
    }

    pub fn remote_status(self: &Rc<Self>, status: Value) {
        remote::status_changed(self, status);
    }

    // MARK: clipboard and links

    pub fn display(&self) -> gdk::Display {
        gdk::Display::default().expect("a display")
    }

    fn copy(&self, key: &PaneKey) {
        if let Some(text) = self.selection_text(key) {
            self.display().clipboard().set_text(&text);
        }
    }

    fn selection_text(&self, key: &PaneKey) -> Option<String> {
        let c = self.core(&key.host)?;
        let resp = c.request_timeout(&json!({"CopySelection": {"pane": key.id}}), 2000);
        resp.get("Text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// The end of a mouse selection: it becomes the primary selection, and with
    /// `copy_on_select`, the clipboard.
    pub fn selection_finished(&self, key: &PaneKey) {
        if let Some(text) = self.selection_text(key) {
            let display = self.display();
            display.primary_clipboard().set_text(&text);
            if self.ui().cfg.terminal.copy_on_select {
                display.clipboard().set_text(&text);
            }
        }
    }

    fn paste(self: &Rc<Self>, key: &PaneKey) {
        let clipboard = self.display().clipboard();
        let k = key.clone();
        let formats = clipboard.formats();
        if !formats.contains_type(glib::GString::static_type())
            && formats.contains_type(gdk::Texture::static_type())
        {
            clipboard.read_texture_async(None::<&gio::Cancellable>, move |res| {
                if let Ok(Some(t)) = res {
                    with_app(|a| a.paste_image(&k, &t));
                }
            });
            return;
        }
        clipboard.read_text_async(None::<&gio::Cancellable>, move |res| {
            if let Ok(Some(text)) = res {
                with_app(|a| a.paste_text(&k, text.to_string()));
            }
        });
    }

    pub fn paste_primary(self: &Rc<Self>, key: &PaneKey) {
        let k = key.clone();
        self.display()
            .primary_clipboard()
            .read_text_async(None::<&gio::Cancellable>, move |res| {
                if let Ok(Some(text)) = res {
                    with_app(|a| a.paste_text(&k, text.to_string()));
                }
            });
    }

    /// Pastes `text`, asking first when it has several lines and the program did not enable
    /// bracketed paste.
    pub fn paste_text(self: &Rc<Self>, key: &PaneKey, text: String) {
        if text.is_empty() {
            return;
        }
        if self.view(key).is_some_and(|v| v.is_offline()) {
            return;
        }
        let bracketed = self
            .view(key)
            .is_some_and(|v| v.snapshot_modes() & crate::render::MODE_BRACKETED_PASTE != 0);
        let multiline = text.contains('\n') || text.contains('\r');
        let k = key.clone();
        let paste = move |text: String| {
            with_app(|a| {
                if let Some(c) = a.core(&k.host) {
                    c.paste(k.id, &text);
                }
            });
        };
        if multiline && !bracketed && self.ui().cfg.security.confirm_multiline_paste {
            let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
            if let Some(w) = self.win() {
                dialogs::confirm(
                    &w.window,
                    &format!("Paste {lines} lines?"),
                    "The program in this pane did not enable bracketed paste, so every line may run as a separate command as soon as it is pasted.",
                    "Paste",
                    false,
                    move || paste(text),
                );
            }
            return;
        }
        paste(text);
    }

    /// An image (pasted or dropped): saved as a PNG on the pane's machine, its path pasted.
    pub fn paste_image(self: &Rc<Self>, key: &PaneKey, texture: &gdk::Texture) {
        let png = texture.save_to_png_bytes();
        let path = if key.is_remote() {
            let result = match self.core(&key.host) {
                Some(c) => c.write_temp_file("paste.png", &png),
                None => Err("not connected".into()),
            };
            match result {
                Ok(p) => p,
                Err(e) => {
                    if let Some(v) = self.view(key) {
                        v.show_toast(
                            &format!("Could not copy the image to {}: {e}", key.host),
                            5.0,
                        );
                    }
                    return;
                }
            }
        } else {
            match integrations::save_drop_image(&png) {
                Some(p) => p,
                None => return,
            }
        };
        self.paste_text(key, model::shell_escape(&path) + " ");
    }

    pub fn open_link(self: &Rc<Self>, url: &str, key: &PaneKey) {
        let host = key.is_remote().then(|| key.host.clone());
        let resp = core::remote_call(&json!({"op": "link", "url": url, "host": host}));
        let action = resp.get("action").and_then(Value::as_str).unwrap_or("ignore");
        let path = resp.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(w) = self.win() else { return };
        match action {
            "open" => {
                let _ = gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>);
            }
            "reveal" => integrations::show_in_file_manager(&path),
            "ask" => {
                let u = url.to_string();
                let title = match &host {
                    Some(h) => format!("Open this link from {h}?"),
                    None => "Open this link?".into(),
                };
                dialogs::confirm(&w.window, &title, url, "Open", false, move || {
                    let _ =
                        gio::AppInfo::launch_default_for_uri(&u, None::<&gio::AppLaunchContext>);
                });
            }
            "copy_path" => {
                let h = host.unwrap_or_default();
                let p = path.clone();
                dialogs::confirm(
                    &w.window,
                    &format!("This is a file on {h}"),
                    &format!("{path}\n\nFiles on a remote host are not opened on this computer."),
                    "Copy Path",
                    false,
                    move || {
                        with_app(|a| a.display().clipboard().set_text(&p));
                    },
                );
            }
            _ => {}
        }
    }

    fn explain(&self) {
        let Some(k) = self.focused_pane() else { return };
        let Some(c) = self.core(&k.host) else { return };
        c.request_async(&json!({"Explain": {"pane": k.id}}), 120_000, |resp| {
            with_app(|a| {
                let Some(w) = a.win() else { return };
                match resp.get("Text").and_then(Value::as_str) {
                    Some(t) => dialogs::inform(&w.window, "Last Command", t),
                    None => dialogs::inform(
                        &w.window,
                        "Could Not Explain the Last Command",
                        resp.get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("The session daemon is not connected."),
                    ),
                }
            });
        });
    }

    // MARK: appearance, config, themes

    fn watch_appearance(self: &Rc<Self>) {
        if let Some(source) = gio::SettingsSchemaSource::default()
            && source.lookup("org.gnome.desktop.interface", true).is_some()
        {
            let settings = gio::Settings::new("org.gnome.desktop.interface");
            settings.connect_changed(Some("color-scheme"), |_, _| {
                with_app(|a| a.appearance_changed());
            });
            // Lives as long as the app.
            std::mem::forget(settings);
        }
        if let Some(s) = gtk::Settings::default() {
            s.connect_gtk_theme_name_notify(|_| {
                with_app(|a| a.appearance_changed());
            });
        }
    }

    fn appearance_changed(&self) {
        let dark = system_is_dark();
        if self.last_dark.get() == Some(dark) {
            return;
        }
        self.last_dark.set(Some(dark));
        for c in self.cores.borrow().values() {
            c.send(&json!({"SetAppearance": {"dark": dark}}));
        }
        if self.ui().follows_appearance() {
            self.reload_config(false);
        }
    }

    pub fn reload_config(&self, notify_daemon: bool) {
        if notify_daemon && let Some(c) = self.core(LOCAL) {
            c.request(&json!("ReloadConfig"));
        }
        let mut ui = UiConfig::load(system_is_dark());
        let mut warnings = Vec::new();
        let bindings = actions::bindings(&ui.cfg.keybindings, &mut warnings);
        ui.warnings.extend(warnings);
        for (name, accels) in &bindings {
            let a: Vec<&str> = accels.iter().map(String::as_str).collect();
            self.gtk_app.set_accels_for_action(&format!("app.{name}"), &a);
        }
        *self.keymap.borrow_mut() = actions::keymap(&bindings);
        let problem = ui.problem();
        *self.ui.borrow_mut() = ui;
        self.apply_config();
        for (h, c) in self.cores.borrow().iter() {
            if h != LOCAL {
                c.reload_engine();
            }
        }
        core::remotes_sync();
        quick::configure_changed(self);
        match problem {
            Some(p) => self.toast(&p, 8.0),
            None if notify_daemon => self.toast("Configuration reloaded", 2.5),
            None => {}
        }
    }

    fn apply_config(&self) {
        let fonts = {
            let ui = self.ui();
            let ctx = self
                .win()
                .map(|w| w.window.pango_context())
                .unwrap_or_else(|| gtk::Label::new(None).pango_context());
            let size = self.font_override.get().unwrap_or(ui.font_size());
            Rc::new(Fonts::new(&ctx, &ui.cfg, &ui.font_features, size))
        };
        *self.fonts.borrow_mut() = fonts.clone();
        for v in self.views() {
            v.set_fonts(fonts.clone());
        }
        if let Some(w) = self.win() {
            w.apply_config(&self.ui());
        }
        if let Some(q) = self.quick.borrow().as_ref() {
            q.apply_config(&self.ui());
        }
        for t in self.all_tabs() {
            t.relayout();
        }
        self.focus_changed();
    }

    fn change_font(&self, delta: f64) {
        let base = self.ui().font_size();
        let cur = self.font_override.get().unwrap_or(base);
        self.font_override.set(if delta == 0.0 {
            None
        } else {
            Some((cur + delta).clamp(6.0, 72.0))
        });
        self.apply_config();
    }

    fn toggle_tab_style(&self) {
        let value = if self.ui().sidebar_tabs() {
            "\"native\""
        } else {
            "\"sidebar\""
        };
        if let Some(c) = self.core(LOCAL) {
            let r = c.request(&json!({"SetSetting": {"key": "window.tab_style", "value": value}}));
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                self.toast(&format!("Tabs: {e}"), 6.0);
            }
        }
    }

    /// View › Theme: Browse, Match System Appearance, and the theme list (own themes inline,
    /// the rest by first letter).
    pub fn theme_menu(&self) -> gio::Menu {
        let ui = self.ui();
        let menu = gio::Menu::new();
        let top = gio::Menu::new();
        top.append(Some("Browse Themes…"), Some("app.browse_themes"));
        top.append(
            Some(if ui.follows_appearance() {
                "✓ Match System Appearance"
            } else {
                "Match System Appearance"
            }),
            Some("app.follow_appearance_toggle"),
        );
        menu.append_section(None, &top);
        let lists: Vec<(Option<&str>, Option<bool>, String)> = if ui.follows_appearance() {
            vec![
                (Some("Light Appearance"), Some(false), ui.theme_spec.light.clone()),
                (Some("Dark Appearance"), Some(true), ui.theme_spec.dark.clone()),
            ]
        } else {
            vec![(None, None, ui.theme.name.clone())]
        };
        for (title, dark, current) in lists {
            let section = gio::Menu::new();
            let mut by_letter: std::collections::BTreeMap<String, gio::Menu> = Default::default();
            for t in ui.themes.iter().filter(|t| dark.is_none_or(|d| t.dark == d)) {
                let label = if t.name == current {
                    format!("✓ {}", t.name)
                } else {
                    t.name.clone()
                };
                let item = gio::MenuItem::new(Some(&label), None);
                item.set_action_and_target_value(Some("app.theme"), Some(&t.name.to_variant()));
                if t.own {
                    section.append_item(&item);
                } else {
                    let first = t.name.chars().next().unwrap_or('#');
                    let key = if first.is_alphabetic() {
                        first.to_uppercase().to_string()
                    } else {
                        "0–9".into()
                    };
                    by_letter
                        .entry(key)
                        .or_default()
                        .append_item(&item);
                }
            }
            let more = gio::Menu::new();
            for (letter, sub) in by_letter {
                more.append_submenu(Some(&letter), &sub);
            }
            section.append_submenu(Some("More Themes"), &more);
            menu.append_section(title, &section);
        }
        menu
    }

    pub fn choose_theme(&self, name: &str) {
        let spec = {
            let ui = self.ui();
            if ui.follows_appearance() {
                let dark = ui.themes.iter().find(|t| t.name == name).is_some_and(|t| t.dark);
                if dark {
                    format!("light:{},dark:{name}", ui.theme_spec.light)
                } else {
                    format!("light:{name},dark:{}", ui.theme_spec.dark)
                }
            } else {
                name.to_string()
            }
        };
        if !self.set_theme(&spec) {
            self.preview_theme(None);
        }
    }

    fn set_theme(&self, spec: &str) -> bool {
        let Some(c) = self.core(LOCAL) else { return false };
        let r = c.request(&json!({"SetTheme": {"spec": spec}}));
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            self.toast(&format!("Theme: {e}"), 6.0);
            return false;
        }
        true
    }

    fn toggle_follow_appearance(&self) {
        let spec = {
            let ui = self.ui();
            if ui.follows_appearance() {
                ui.theme.name.clone()
            } else {
                let dark = ui.theme_spec.dark.clone();
                let light = match dark.as_str() {
                    "catppuccin-mocha" => "catppuccin-latte",
                    _ => "thurm-light",
                };
                let dark = if ui.themes.iter().any(|t| t.name == dark && t.dark) {
                    dark
                } else {
                    "thurm".into()
                };
                format!("light:{light},dark:{dark}")
            }
        };
        self.set_theme(&spec);
    }

    fn browse_themes(self: &Rc<Self>) {
        let Some(win) = self.win() else { return };
        let (items, initial) = {
            let ui = self.ui();
            let current = if ui.follows_appearance() {
                ui.theme_spec.name(ui.dark).to_string()
            } else {
                ui.theme.name.clone()
            };
            let mut items = Vec::new();
            let mut initial = 0;
            for (i, t) in ui.themes.iter().enumerate() {
                if t.name == current {
                    initial = i;
                }
                let detail = format!(
                    "{}{}",
                    if t.dark { "Dark" } else { "Light" },
                    if t.own { " · Thurm" } else { "" }
                );
                let n1 = t.name.clone();
                let n2 = t.name.clone();
                let mut item = Item::new(t.name.clone(), detail, move || {
                    with_app(|a| a.choose_theme(&n1));
                });
                if t.name == current {
                    item.shortcut = "current".into();
                }
                item.preview = Some(Rc::new(move || {
                    with_app(|a| a.preview_theme(Some(&n2)));
                }));
                items.push(item);
            }
            (items, initial)
        };
        let n = items.len();
        palette::show(
            &win,
            items,
            &format!("Search {n} themes…"),
            Some("↑↓ preview · ↩ keep · esc cancel"),
            initial,
            Some(Rc::new(|| {
                with_app(|a| a.preview_theme(None));
            })),
        );
    }

    fn preview_theme(&self, name: Option<&str>) {
        let Some(c) = self.core(LOCAL) else { return };
        for (h, rc) in self.cores.borrow().iter() {
            if h != LOCAL {
                let _ = rc.preview_theme(name);
            }
        }
        let Some(theme) = c.preview_theme(name) else { return };
        if let Ok(t) = serde_json::from_value::<thurm_config::Theme>(theme) {
            self.ui.borrow_mut().theme = t;
            if let Some(w) = self.win() {
                w.apply_config(&self.ui());
            }
            for v in self.views() {
                v.area.queue_draw();
            }
        }
    }

    pub fn integrations_menu(&self) -> gio::Menu {
        integrations::menu()
    }

    pub fn toggle_quick(self: &Rc<Self>) {
        quick::toggle(self);
    }
}

// MARK: helpers

fn tab_shortcut(index: usize, count: usize) -> Option<usize> {
    if index < 8 {
        Some(index + 1)
    } else if index + 1 == count {
        Some(9)
    } else {
        None
    }
}

pub fn home() -> Option<String> {
    std::env::var("HOME").ok()
}

fn relative_time(secs: u64) -> String {
    let d = model::now_secs().saturating_sub(secs);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min. ago", d / 60),
        3600..=86399 => format!("{} hr. ago", d / 3600),
        _ => format!("{} days ago", d / 86400),
    }
}

/// Every (host, id) leaf of a stored tree.
pub fn leaves(node: &thurm_proto::layout::LayoutNode) -> Vec<(Option<&str>, PaneId)> {
    let mut out = Vec::new();
    node.leaves(&mut out);
    out
}

/// The tree with only the leaves `keep` accepts.
pub fn retain_node(
    node: thurm_proto::layout::LayoutNode,
    keep: &dyn Fn(Option<&str>, PaneId) -> bool,
) -> Option<thurm_proto::layout::LayoutNode> {
    use thurm_proto::layout::LayoutNode;
    match node {
        LayoutNode::Pane { id, host } => {
            keep(host.as_deref(), id).then_some(LayoutNode::Pane { id, host })
        }
        LayoutNode::Split {
            dir,
            ratio,
            first,
            second,
        } => match (retain_node(*first, keep), retain_node(*second, keep)) {
            (Some(a), Some(b)) => Some(LayoutNode::Split {
                dir,
                ratio,
                first: Box::new(a),
                second: Box::new(b),
            }),
            (a, b) => a.or(b),
        },
    }
}

pub fn retain_tabs(tabs: &mut Vec<TabLayout>, keep: &dyn Fn(Option<&str>, PaneId) -> bool) {
    tabs.retain_mut(|t| match retain_node(t.root.clone(), keep) {
        Some(r) => {
            let ids: Vec<PaneId> = leaves(&r).iter().map(|(_, id)| *id).collect();
            if !ids.contains(&t.focused) {
                t.focused = ids[0];
            }
            if t.zoomed.is_some_and(|z| !ids.contains(&z)) {
                t.zoomed = None;
            }
            t.root = r;
            true
        }
        None => false,
    });
}

/// Drops the panes `keep` rejects from every window, workspace and the quick terminal.
fn retain_layout(layout: &mut Layout, keep: &dyn Fn(Option<&str>, PaneId) -> bool) {
    for w in &mut layout.windows {
        retain_tabs(&mut w.tabs, keep);
        w.selected_tab = w.selected_tab.min(w.tabs.len().saturating_sub(1));
    }
    layout.windows.retain(|w| !w.tabs.is_empty());
    for ws in &mut layout.workspaces {
        // A workspace's leaves without a host belong to its host.
        let host = ws.host.clone();
        let k = |h: Option<&str>, id: PaneId| keep(h.or(host.as_deref()), id);
        retain_tabs(&mut ws.tabs, &k);
        ws.selected_tab = ws.selected_tab.min(ws.tabs.len().saturating_sub(1));
    }
    let shown: Vec<u64> = layout.windows.iter().map(|w| w.workspace).collect();
    layout
        .workspaces
        .retain(|w| !w.tabs.is_empty() || shown.contains(&w.id));
    if let Some(q) = layout.quick.take() {
        let mut v = vec![q];
        retain_tabs(&mut v, keep);
        layout.quick = v.pop();
    }
}

/// The layout as JSON with sorted keys (compared with the last one sent).
fn sorted_json(layout: &Layout) -> String {
    sort_value(serde_json::to_value(layout).unwrap_or(Value::Null)).to_string()
}

fn sort_value(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(String, Value)> = m.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sort_value(v))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sort_value).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thurm_proto::layout::LayoutNode;

    #[test]
    fn shortcuts_by_index() {
        assert_eq!(tab_shortcut(0, 3), Some(1));
        assert_eq!(tab_shortcut(7, 12), Some(8));
        assert_eq!(tab_shortcut(11, 12), Some(9));
        assert_eq!(tab_shortcut(9, 12), None);
    }

    #[test]
    fn retain_drops_dead_panes_and_fixes_focus() {
        let mut layout = Layout {
            windows: vec![WindowLayout {
                frame: None,
                tabs: vec![TabLayout {
                    title: None,
                    root: LayoutNode::Split {
                        dir: thurm_proto::layout::SplitDir::Right,
                        ratio: 0.5,
                        first: Box::new(LayoutNode::local(1)),
                        second: Box::new(LayoutNode::local(2)),
                    },
                    focused: 2,
                    zoomed: Some(2),
                    handoff: None,
                }],
                selected_tab: 0,
                fullscreen: false,
                workspace: 1,
            }],
            workspaces: vec![],
            quick: None,
        };
        retain_layout(&mut layout, &|_, id| id == 1);
        let t = &layout.windows[0].tabs[0];
        assert_eq!(t.root, LayoutNode::local(1));
        assert_eq!(t.focused, 1);
        assert_eq!(t.zoomed, None);
    }

    #[test]
    fn json_keys_are_sorted() {
        let s = sorted_json(&Layout::default());
        assert!(s.starts_with("{\"windows\""), "{s}");
        assert!(s.find("\"windows\"") < s.find("\"workspaces\""));
    }
}
