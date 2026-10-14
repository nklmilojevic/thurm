//! The window: header bar (sidebar toggle, new tab, title, main menu), the tab sidebar or tab
//! bar (`window.tab_style`), the tabs, and the overlay for the command palette. Colors follow
//! the terminal theme, like the macOS window chrome.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::actions::{self, ACTIONS, Menu};
use crate::app;
use crate::config::{UiConfig, blend, css_rgb, css_rgba, surface_shade};
use crate::core::PaneKey;
use crate::sidebar::Sidebar;
use crate::tab::Tab;

pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub tab_view: adw::TabView,
    tab_bar: adw::TabBar,
    pub sidebar: Rc<Sidebar>,
    paned: gtk::Paned,
    title: adw::WindowTitle,
    header: adw::HeaderBar,
    toolbar: adw::ToolbarView,
    sidebar_button: gtk::ToggleButton,
    /// Full height (vertical tabs with pane headers): the header bar sits over the sidebar and
    /// the panes run to the top of the window.
    full_height: std::cell::Cell<bool>,
    /// With the sidebar hidden, the top-left pane's header holds these (window controls,
    /// sidebar, new tab, menu)...
    lead_controls: gtk::Box,
    /// ...and the top-right one the window controls.
    trail_controls: gtk::WindowControls,
    /// The panes whose headers hold them.
    placed: RefCell<Option<(Option<PaneKey>, Option<PaneKey>)>>,
    /// Bumped by every placement and reset, so a queued placement that is no longer the latest
    /// does nothing.
    placement: Rc<std::cell::Cell<u64>>,
    /// The palette and other window-wide overlays go here.
    pub overlay: gtk::Overlay,
    pub tabs: RefCell<Vec<Rc<Tab>>>,
    css: gtk::CssProvider,
    width_save: RefCell<Option<glib::SourceId>>,
    /// The `[x, y]` of the stored frame (Wayland cannot place windows; kept for macOS).
    pub frame_origin: std::cell::Cell<(f64, f64)>,
}

impl MainWindow {
    pub fn new(gtk_app: &adw::Application, ui: &UiConfig) -> Rc<MainWindow> {
        let window = adw::ApplicationWindow::new(gtk_app);
        window.set_title(Some("Thurm"));
        window.set_size_request(240, 140);

        let title = adw::WindowTitle::new("Thurm", "");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));
        let sidebar_button = gtk::ToggleButton::new();
        sidebar_button.set_icon_name("sidebar-show-symbolic");
        sidebar_button.set_tooltip_text(Some("Show or Hide Sidebar (Ctrl+Shift+B)"));
        sidebar_button.set_active(true);
        header.pack_start(&sidebar_button);
        let new_tab = gtk::Button::from_icon_name("tab-new-symbolic");
        new_tab.set_tooltip_text(Some("New Tab (Ctrl+Shift+T)"));
        new_tab.set_action_name(Some("app.new_tab"));
        header.pack_start(&new_tab);
        let menu_button = main_menu_button();
        menu_button.set_primary(true);
        header.pack_end(&menu_button);

        let lead_controls = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        lead_controls.append(&gtk::WindowControls::new(gtk::PackType::Start));
        let show_sidebar = gtk::Button::from_icon_name("sidebar-show-symbolic");
        show_sidebar.set_tooltip_text(Some("Show Sidebar (Ctrl+Shift+B)"));
        show_sidebar.set_action_name(Some("app.toggle_sidebar"));
        let new_tab2 = gtk::Button::from_icon_name("tab-new-symbolic");
        new_tab2.set_tooltip_text(Some("New Tab (Ctrl+Shift+T)"));
        new_tab2.set_action_name(Some("app.new_tab"));
        for b in [&show_sidebar, &new_tab2] {
            b.add_css_class("flat");
            b.add_css_class("thurm-window-button");
            lead_controls.append(b);
        }
        let menu2 = main_menu_button();
        menu2.add_css_class("flat");
        menu2.add_css_class("thurm-window-button");
        lead_controls.append(&menu2);
        let trail_controls = gtk::WindowControls::new(gtk::PackType::End);

        let tab_view = adw::TabView::new();
        tab_view.set_shortcuts(adw::TabViewShortcuts::empty());
        tab_view.set_vexpand(true);
        tab_view.set_hexpand(true);
        let tab_bar = adw::TabBar::new();
        tab_bar.set_view(Some(&tab_view));
        tab_bar.set_autohide(false);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&tab_bar);
        content.append(&tab_view);

        let sidebar = Sidebar::new();
        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&sidebar.root));
        paned.set_end_child(Some(&content));
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);
        paned.set_resize_end_child(true);
        paned.set_shrink_end_child(false);
        paned.set_wide_handle(false);
        paned.set_position(ui.sidebar_width() as i32);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&paned));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&overlay));
        window.set_content(Some(&toolbar));

        let css = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("a display"),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let win = Rc::new(MainWindow {
            window,
            tab_view,
            tab_bar,
            sidebar,
            paned,
            title,
            header,
            toolbar,
            sidebar_button,
            full_height: std::cell::Cell::new(false),
            lead_controls,
            trail_controls,
            placed: RefCell::new(None),
            placement: Rc::new(std::cell::Cell::new(0)),
            overlay,
            tabs: RefCell::new(Vec::new()),
            css,
            width_save: RefCell::new(None),
            frame_origin: std::cell::Cell::new((0.0, 0.0)),
        });
        win.install();
        win.apply_config(ui);
        win
    }

    fn install(self: &Rc<Self>) {
        // Thurm is one window: tabs never detach into new ones.
        self.tab_view.connect_create_window(|_| None);
        self.tab_view.connect_close_page(|view, page| {
            let handled = app::with_app(|a| a.close_tab_page(page)).unwrap_or(false);
            if !handled {
                view.close_page_finish(page, true);
            }
            glib::Propagation::Stop
        });
        self.tab_view.connect_selected_page_notify(|_| {
            glib::idle_add_local_once(|| {
                app::with_app(|a| a.tab_selected());
            });
        });
        self.tab_view.connect_page_reordered(|_, _, _| {
            app::with_app(|a| a.tabs_reordered());
        });
        let weak = Rc::downgrade(self);
        self.sidebar_button.connect_toggled(move |b| {
            if let Some(w) = weak.upgrade() {
                w.sidebar.root.set_visible(b.is_active());
                w.replace_controls();
                app::with_app(|a| a.sidebar_toggled());
            }
        });
        // The sidebar's width is saved to the config a second after a drag.
        let weak = Rc::downgrade(self);
        self.paned.connect_position_notify(move |p| {
            let Some(w) = weak.upgrade() else { return };
            let pos = p.position();
            let clamped = pos.clamp(180, 480);
            if clamped != pos {
                p.set_position(clamped);
                return;
            }
            if let Some(id) = w.width_save.borrow_mut().take() {
                id.remove();
            }
            let weak = Rc::downgrade(&w);
            let id = glib::timeout_add_local_once(Duration::from_secs(1), move || {
                if let Some(w) = weak.upgrade() {
                    w.width_save.borrow_mut().take();
                    if w.sidebar.root.is_visible() {
                        app::with_app(|a| a.sidebar_width_changed(w.paned.position() as f64));
                    }
                }
            });
            *w.width_save.borrow_mut() = Some(id);
        });
        self.window.connect_close_request(|w| {
            // The shells keep running in thurmd either way. With `quit_after_last_window` off
            // (the default) Thurm stays running without a window, like on macOS: the
            // quick-terminal hotkey works, and opening Thurm again shows the window at once.
            app::with_app(|a| {
                if a.ui().cfg.window.quit_after_last_window {
                    a.quit();
                } else {
                    a.save_now(true);
                    w.set_visible(false);
                }
            });
            glib::Propagation::Stop
        });
        self.window.connect_is_active_notify(|_| {
            app::with_app(|a| a.window_activity_changed());
        });
        self.window.connect_fullscreened_notify(|_| {
            app::with_app(|a| a.schedule_save());
        });
        self.window.connect_default_width_notify(|_| {
            app::with_app(|a| a.schedule_save());
        });
    }

    pub fn apply_config(&self, ui: &UiConfig) {
        let sidebar = ui.sidebar_tabs();
        self.set_full_height(sidebar && ui.pane_headers());
        self.tab_bar.set_visible(!sidebar);
        self.sidebar.root.set_visible(sidebar && self.sidebar_button.is_active());
        self.sidebar_button.set_visible(sidebar);
        self.sidebar.set_agent_rows(ui.cfg.window.sidebar_agent_rows);
        if sidebar && (self.paned.position() - ui.sidebar_width() as i32).abs() > 1 {
            self.paned.set_position(ui.sidebar_width() as i32);
        }
        let theme = &ui.theme;
        let dark = ui.theme_is_dark();
        adw::StyleManager::default().set_color_scheme(if dark {
            adw::ColorScheme::ForceDark
        } else {
            adw::ColorScheme::ForceLight
        });
        let bg = theme.background;
        let fg = theme.foreground;
        let opacity = ui.opacity();
        let sidebar_bg = blend(bg, 0x000000, surface_shade(dark));
        let divider = blend(bg, fg, 0.25);
        let font = &ui.cfg.font.family;
        let size = ui.font_size();
        let css = format!(
            r#"
            window.thurm, window.thurm .thurm-content {{ background-color: {bg_a}; }}
            window.thurm headerbar {{ background-color: {bg_a}; color: {fg}; box-shadow: none; }}
            window.thurm.thurm-quick, window.thurm.thurm-quick .thurm-content {{ background-color: {quick_a}; }}
            .thurm-sidebar {{ background-color: {side}; color: {fg}; }}
            .thurm-sidebar list {{ background: transparent; }}
            .thurm-sidebar row {{ border-radius: 6px; margin: 1px 6px; }}
            .thurm-sidebar row:selected {{ background-color: {sel}; }}
            .thurm-sidebar .group-header {{ font-size: 11px; font-weight: 600; opacity: 0.6; margin: 6px 12px 2px 12px; }}
            .thurm-sidebar .row-title {{ font-size: 13px; }}
            .thurm-sidebar .row-title.selected {{ font-weight: 500; }}
            .thurm-sidebar .row-subtitle {{ font-size: 11px; font-feature-settings: "tnum"; opacity: 0.65; }}
            .thurm-sidebar .row-shortcut {{ font-size: 11px; opacity: 0.4; }}
            .thurm-sidebar .agents-header {{ font-size: 11px; font-weight: 600; opacity: 0.6; }}
            .thurm-sidebar .agents-summary {{ font-size: 11px; opacity: 0.45; }}
            .thurm-tab {{ background-color: {divider}; }}
            paned.thurm-main > separator {{ background-color: {divider}; min-width: 1px; }}
            .thurm-toast {{ background-color: rgba(26,26,26,0.75); color: white; font-size: 11px; font-weight: 500; padding: 4px 10px; border-radius: 7px; }}
            .thurm-offline {{ background-color: rgba(0,0,0,0.35); }}
            .thurm-offline-box {{ background-color: rgba(26,26,26,0.85); border-radius: 9px; padding: 8px 12px; }}
            .thurm-offline-label {{ color: white; font-size: 12px; font-weight: 500; }}
            .thurm-lock {{ color: #ff9f0a; }}
            .thurm-findbar {{ background-color: {bg}; color: {fg}; border: 1px solid {border}; border-radius: 8px; padding: 4px 6px; box-shadow: 0 2px 6px rgba(0,0,0,0.25); }}
            .thurm-completion, .thurm-palette {{ background-color: {bg}; color: {fg}; border: 1px solid {border}; border-radius: 8px; padding: 4px; box-shadow: 0 4px 16px rgba(0,0,0,0.3); }}
            .thurm-palette {{ border-radius: 10px; padding: 0; }}
            .thurm-completion list, .thurm-palette list {{ background: transparent; color: {fg}; }}
            .thurm-completion row, .thurm-palette row {{ border-radius: 5px; margin: 1px 2px; padding: 2px 4px; }}
            .thurm-completion row:selected, .thurm-palette row:selected {{ background-color: {selbg}; color: {selfg}; }}
            .thurm-completion label, .thurm-palette label {{ font-family: "{font}"; font-size: {size}px; }}
            .thurm-completion .thurm-completion-detail, .thurm-palette .detail {{ font-size: {small}px; }}
            .thurm-palette entry {{ font-family: "{font}"; font-size: {big}px; background: transparent; border: none; box-shadow: none; outline: none; padding: 10px 14px; }}
            .thurm-palette .footer {{ font-size: {small}px; opacity: 0.55; margin: 4px 16px 8px 16px; }}
            .thurm-sidebar headerbar {{ background-color: transparent; box-shadow: none; }}
            .thurm-pane-header {{ background-color: {bg_a}; color: {fg}; }}
            .thurm-card-top {{ border-radius: 7px 7px 0 0; }}
            .thurm-card-bottom {{ border-radius: 0 0 7px 7px; }}
            .thurm-pane-title {{ font-size: 13px; font-weight: 600; opacity: 0.6; }}
            .thurm-pane-header.focused .thurm-pane-title {{ opacity: 1; }}
            .thurm-pane-icon {{ opacity: 0.55; }}
            .thurm-pane-header.focused .thurm-pane-icon {{ opacity: 0.9; }}
            button.thurm-pane-button {{ min-width: 22px; min-height: 22px; padding: 0; opacity: 0.45; }}
            .thurm-pane-header:not(.focused) button.thurm-pane-button {{ opacity: 0.3; }}
            .thurm-pane-header windowcontrols button {{ min-width: 22px; min-height: 22px; padding: 0; }}
            button.thurm-window-button {{ min-width: 26px; min-height: 26px; padding: 0; }}
            "#,
            bg_a = css_rgba(bg, opacity),
            quick_a = css_rgba(bg, ui.quick_opacity()),
            bg = css_rgb(bg),
            fg = css_rgb(fg),
            side = css_rgba(sidebar_bg, 1.0),
            sel = css_rgba(fg, 0.12),
            divider = css_rgb(divider),
            border = css_rgba(fg, 0.14),
            selbg = css_rgb(theme.selection_background),
            selfg = css_rgb(theme.selection_foreground),
            font = font,
            size = size.round(),
            small = (size - 2.0).max(8.0).round(),
            big = (size + 2.0).round(),
        );
        self.css.load_from_string(&css);
        self.window.add_css_class("thurm");
        self.paned.add_css_class("thurm-main");
    }

    /// Moves the header bar over the sidebar (full height) or back above everything.
    fn set_full_height(&self, full: bool) {
        if self.full_height.replace(full) == full && self.header.parent().is_some() {
            return;
        }
        if self.header.parent().is_some() {
            if self.header.parent().as_ref() == Some(self.sidebar.root.upcast_ref()) {
                self.sidebar.root.remove(&self.header);
            } else {
                self.toolbar.remove(&self.header);
            }
        }
        if full {
            self.sidebar.root.prepend(&self.header);
        } else {
            self.toolbar.add_top_bar(&self.header);
        }
        self.header.set_show_end_title_buttons(!full);
        self.title.set_visible(!full);
        self.replace_controls();
    }

    /// Takes the controls out of the pane headers; the selected tab puts them back on its next
    /// layout.
    fn replace_controls(&self) {
        for w in [self.lead_controls.upcast_ref::<gtk::Widget>(), self.trail_controls.upcast_ref()] {
            if let Some(p) = w.parent().and_downcast::<gtk::Box>() {
                p.remove(w);
            }
        }
        *self.placed.borrow_mut() = None;
        self.placement.set(self.placement.get() + 1);
        if let Some(t) = self.selected_tab() {
            t.relayout();
        }
    }

    /// Called from the tab's layout: puts the window controls into the headers of its top-left
    /// (`lead`, when the sidebar is hidden) and top-right (`trail`) panes, after the layout.
    pub fn place_controls_later(&self, tab: std::rc::Weak<Tab>, lead: Option<PaneKey>, trail: Option<PaneKey>) {
        let Some(t) = tab.upgrade() else { return };
        if !self.full_height.get() || !self.selected_tab().is_some_and(|s| Rc::ptr_eq(&s, &t)) {
            return;
        }
        let lead = lead.filter(|_| !self.sidebar_shown());
        let want = (lead, trail);
        if self.placed.borrow().as_ref() == Some(&want) {
            return;
        }
        *self.placed.borrow_mut() = Some(want.clone());
        let generation = self.placement.get() + 1;
        self.placement.set(generation);
        let current = self.placement.clone();
        let lead_w = self.lead_controls.clone();
        let trail_w = self.trail_controls.clone();
        glib::idle_add_local_once(move || {
            // A mode change, sidebar toggle or newer layout came first.
            if current.get() != generation {
                return;
            }
            for w in [lead_w.upcast_ref::<gtk::Widget>(), trail_w.upcast_ref()] {
                if let Some(p) = w.parent().and_downcast::<gtk::Box>() {
                    p.remove(w);
                }
            }
            if let Some(h) = want.0.as_ref().and_then(|k| t.header(k)) {
                h.lead.append(&lead_w);
            }
            if let Some(h) = want.1.as_ref().and_then(|k| t.header(k)) {
                h.trail.append(&trail_w);
            }
        });
    }

    pub fn set_title(&self, title: &str, subtitle: &str) {
        self.title.set_title(title);
        self.title.set_subtitle(subtitle);
        self.window.set_title(Some(title));
    }

    pub fn sidebar_shown(&self) -> bool {
        self.sidebar.root.is_visible()
    }

    pub fn toggle_sidebar(&self) {
        self.sidebar_button.set_active(!self.sidebar_button.is_active());
    }

    /// Tabs in tab-view order.
    pub fn ordered_tabs(&self) -> Vec<Rc<Tab>> {
        let tabs = self.tabs.borrow();
        (0..self.tab_view.n_pages())
            .filter_map(|i| {
                let page = self.tab_view.nth_page(i);
                tabs.iter().find(|t| t.root.upcast_ref::<gtk::Widget>() == &page.child()).cloned()
            })
            .collect()
    }

    pub fn tab_for_page(&self, page: &adw::TabPage) -> Option<Rc<Tab>> {
        let child = page.child();
        self.tabs
            .borrow()
            .iter()
            .find(|t| t.root.upcast_ref::<gtk::Widget>() == &child)
            .cloned()
    }

    pub fn selected_tab(&self) -> Option<Rc<Tab>> {
        self.tab_view.selected_page().and_then(|p| self.tab_for_page(&p))
    }

    /// Adds `tab` after the selected one (or at the end).
    pub fn add_tab(&self, tab: &Rc<Tab>, select: bool) {
        let pos = self
            .tab_view
            .selected_page()
            .map(|p| self.tab_view.page_position(&p) + 1)
            .unwrap_or(self.tab_view.n_pages());
        let page = self.tab_view.insert(&tab.root, pos);
        *tab.page.borrow_mut() = Some(page.clone());
        self.tabs.borrow_mut().push(tab.clone());
        if select {
            self.tab_view.set_selected_page(&page);
        }
    }

    /// Removes the tab's page without asking (its panes keep running unless closed already).
    pub fn remove_tab(&self, tab: &Rc<Tab>) {
        self.tabs.borrow_mut().retain(|t| !Rc::ptr_eq(t, tab));
        if let Some(page) = tab.page.borrow_mut().take() {
            self.tab_view.close_page(&page);
        }
    }

    pub fn select_tab(&self, tab: &Rc<Tab>) {
        if let Some(page) = tab.page.borrow().as_ref() {
            self.tab_view.set_selected_page(page);
        }
    }
}

fn main_menu_button() -> gtk::MenuButton {
    let b = gtk::MenuButton::new();
    b.set_icon_name("open-menu-symbolic");
    b.set_tooltip_text(Some("Main Menu"));
    b.set_create_popup_func(|b| {
        let model = app::with_app(|a| main_menu(a)).unwrap_or_else(gio::Menu::new);
        b.set_menu_model(Some(&model));
    });
    b
}

/// The main menu, as the macOS menu bar: one submenu per menu, rebuilt on every open so titles
/// and checkmarks are current.
pub fn main_menu(a: &app::App) -> gio::Menu {
    let menu = gio::Menu::new();
    for m in [Menu::Shell, Menu::Edit, Menu::View, Menu::Window, Menu::App, Menu::Help] {
        let sub = gio::Menu::new();
        let mut section = gio::Menu::new();
        for def in ACTIONS.iter().filter(|d| d.menu == m && !actions::hidden_from_menu(d.name)) {
            if !a.action_visible(def.name) {
                continue;
            }
            if matches!(def.name, "about" | "quit" | "split_right" | "copy" | "increase_font_size" | "zoom_split" | "previous_tab" | "move_tab_to_new_workspace")
                && section.n_items() > 0
            {
                sub.append_section(None, &section);
                section = gio::Menu::new();
            }
            let label = a.action_label(def.name);
            if def.name == "browse_themes" {
                section.append_submenu(Some("Theme"), &a.theme_menu());
                continue;
            }
            if def.name == "follow_appearance" {
                continue;
            }
            section.append(Some(&label), Some(&format!("app.{}", def.name)));
        }
        if m == Menu::Window {
            section.append_submenu(Some("Workspaces"), &a.workspace_menu(false));
        }
        if m == Menu::App {
            section.append_submenu(Some("Integrations"), &a.integrations_menu());
        }
        if section.n_items() > 0 {
            sub.append_section(None, &section);
        }
        menu.append_submenu(Some(m.title()), &sub);
    }
    menu
}
