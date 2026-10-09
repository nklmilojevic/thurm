//! The tab sidebar (TabSidebar.swift): the workspace button, the window's tabs grouped by git
//! repository, and the agents panel.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use thurm_proto::AgentStatus;

use crate::app;
use crate::core::PaneKey;
use crate::model;
use crate::tab::Tab;

#[derive(Clone, PartialEq)]
pub struct TabRow {
    pub title: String,
    /// Pango markup.
    pub subtitle: String,
    pub status: Option<AgentStatus>,
    pub shortcut: String,
    pub selected: bool,
    pub tooltip: String,
}

#[derive(Clone, PartialEq)]
pub struct AgentRow {
    pub key: PaneKey,
    pub title: String,
    pub subtitle: String,
    pub status: Option<AgentStatus>,
    pub selected: bool,
}

#[derive(Clone, PartialEq)]
pub struct Group {
    /// `None` when there is only one group (no headers).
    pub name: Option<String>,
    pub rows: Vec<TabRow>,
}

pub struct Sidebar {
    pub root: gtk::Box,
    workspace_label: gtk::Label,
    list: gtk::ListBox,
    agents_box: gtk::Box,
    agents_list: gtk::ListBox,
    agents_summary: gtk::Label,
    agents_scroll: gtk::ScrolledWindow,
    shown: RefCell<(Vec<Group>, Vec<AgentRow>, Vec<String>)>,
    /// Row index → tab, for clicks, drags and menus.
    row_tabs: RefCell<Vec<Option<Weak<Tab>>>>,
    agent_keys: RefCell<Vec<PaneKey>>,
    /// Agents shown before the panel scrolls (`window.sidebar_agent_rows`).
    agent_rows: Cell<u32>,
    /// An agent row's height as laid out (its content can be taller than ROW_HEIGHT).
    agent_row_height: Cell<i32>,
}

const ROW_HEIGHT: i32 = 40;
/// Kept for the tab list (and the workspace button) when the agents panel is dragged taller.
const MIN_TABS_HEIGHT: i32 = 160;

impl Sidebar {
    pub fn new() -> Rc<Sidebar> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("thurm-sidebar");
        root.set_width_request(180);

        let workspace_label = gtk::Label::new(Some("Workspace"));
        workspace_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        workspace_label.add_css_class("heading");
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        inner.append(&workspace_label);
        inner.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
        let workspace = gtk::MenuButton::new();
        workspace.set_child(Some(&inner));
        workspace.add_css_class("flat");
        workspace.set_tooltip_text(Some("Workspaces (Ctrl+Alt+O)"));
        workspace.set_halign(gtk::Align::Start);
        workspace.set_margin_start(10);
        workspace.set_margin_top(4);
        workspace.set_create_popup_func(|b| {
            let model = app::with_app(|a| a.workspace_menu(true)).unwrap_or_else(gio::Menu::new);
            b.set_menu_model(Some(&model));
        });
        root.append(&workspace);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));
        scroll.set_margin_top(4);
        root.append(&scroll);

        let agents_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // The separator and the header are the handle that drags the panel taller or shorter.
        let handle = gtk::Box::new(gtk::Orientation::Vertical, 0);
        handle.set_cursor_from_name(Some("ns-resize"));
        handle.set_tooltip_text(Some("Drag to show more or fewer agents"));
        handle.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header.set_margin_start(16);
        header.set_margin_end(14);
        header.set_margin_top(8);
        header.set_margin_bottom(4);
        let title = gtk::Label::new(Some("Agents"));
        title.add_css_class("agents-header");
        title.set_hexpand(true);
        title.set_xalign(0.0);
        let agents_summary = gtk::Label::new(None);
        agents_summary.add_css_class("agents-summary");
        header.append(&title);
        header.append(&agents_summary);
        handle.append(&header);
        agents_box.append(&handle);
        let agents_list = gtk::ListBox::new();
        agents_list.set_selection_mode(gtk::SelectionMode::Single);
        let agents_scroll = gtk::ScrolledWindow::new();
        agents_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        agents_scroll.set_propagate_natural_height(true);
        agents_scroll.set_child(Some(&agents_list));
        agents_scroll.set_margin_bottom(8);
        agents_box.append(&agents_scroll);
        agents_box.set_visible(false);
        root.append(&agents_box);

        let sidebar = Rc::new(Sidebar {
            root,
            workspace_label,
            list,
            agents_box,
            agents_list,
            agents_summary,
            agents_scroll,
            shown: RefCell::new((Vec::new(), Vec::new(), Vec::new())),
            row_tabs: RefCell::new(Vec::new()),
            agent_keys: RefCell::new(Vec::new()),
            agent_rows: Cell::new(0),
            agent_row_height: Cell::new(ROW_HEIGHT),
        });
        sidebar.set_agent_rows(5);

        let drag = gtk::GestureDrag::new();
        let start = Rc::new(Cell::new(0u32));
        let weak = Rc::downgrade(&sidebar);
        drag.connect_drag_begin({
            let start = start.clone();
            move |_, _, _| {
                if let Some(s) = weak.upgrade() {
                    start.set(s.agent_rows.get());
                }
            }
        });
        let weak = Rc::downgrade(&sidebar);
        drag.connect_drag_update({
            let start = start.clone();
            move |_, _, dy| {
                let Some(s) = weak.upgrade() else { return };
                let h = s.agent_row_height.get();
                let fit = ((s.root.height() - MIN_TABS_HEIGHT) / h).max(1) as u32;
                let rows = start.get() as i64 - (dy / h as f64).round() as i64;
                s.set_agent_rows(rows.clamp(1, fit.max(start.get()) as i64) as u32);
            }
        });
        let weak = Rc::downgrade(&sidebar);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(s) = weak.upgrade() {
                let rows = s.agent_rows.get();
                app::with_app(|a| a.agent_rows_changed(rows));
            }
        });
        handle.add_controller(drag);

        let weak = Rc::downgrade(&sidebar);
        sidebar.list.connect_row_activated(move |_, row| {
            let Some(s) = weak.upgrade() else { return };
            let tab = s
                .row_tabs
                .borrow()
                .get(row.index() as usize)
                .cloned()
                .flatten()
                .and_then(|w| w.upgrade());
            match tab {
                Some(t) => {
                    app::with_app(|a| a.select_tab(&t));
                }
                None => {
                    // A group header: collapse or expand it.
                    if let Some(name) = row.widget_name().strip_prefix("group:") {
                        app::with_app(|a| a.toggle_group(name));
                    }
                }
            }
        });
        let weak = Rc::downgrade(&sidebar);
        sidebar.agents_list.connect_row_activated(move |_, row| {
            let Some(s) = weak.upgrade() else { return };
            let key = s.agent_keys.borrow().get(row.index() as usize).cloned();
            if let Some(k) = key {
                app::with_app(|a| a.focus_agent(&k));
            }
        });
        sidebar
    }

    pub fn set_agent_rows(&self, rows: u32) {
        let rows = rows.clamp(1, 100);
        if rows != self.agent_rows.get() {
            self.agent_rows.set(rows);
            self.fit_agents();
        }
    }

    fn fit_agents(&self) {
        self.agents_scroll
            .set_max_content_height(self.agent_rows.get() as i32 * self.agent_row_height.get());
    }

    pub fn set_workspace_name(&self, name: &str) {
        self.workspace_label.set_text(name);
    }

    /// Shows the rows; nothing is rebuilt when they did not change (agents retitle often).
    pub fn update(
        self: &Rc<Self>,
        groups: Vec<Group>,
        tabs: Vec<Vec<Rc<Tab>>>,
        collapsed: &[String],
        agents: Vec<AgentRow>,
    ) {
        let unchanged = {
            let shown = self.shown.borrow();
            shown.0 == groups && shown.1 == agents && shown.2 == collapsed
        };
        if !unchanged {
            while let Some(r) = self.list.first_child() {
                self.list.remove(&r);
            }
            let mut row_tabs = Vec::new();
            for (g, group_tabs) in groups.iter().zip(&tabs) {
                let is_collapsed = g.name.as_ref().is_some_and(|n| collapsed.contains(n));
                if let Some(name) = &g.name {
                    let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                    let arrow = gtk::Image::from_icon_name(if is_collapsed {
                        "pan-end-symbolic"
                    } else {
                        "pan-down-symbolic"
                    });
                    let label = gtk::Label::new(Some(name));
                    label.set_xalign(0.0);
                    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                    header.append(&arrow);
                    header.append(&label);
                    header.add_css_class("group-header");
                    let row = gtk::ListBoxRow::new();
                    row.set_child(Some(&header));
                    row.set_selectable(false);
                    row.set_widget_name(&format!("group:{name}"));
                    self.list.append(&row);
                    row_tabs.push(None);
                }
                if is_collapsed {
                    continue;
                }
                for (r, tab) in g.rows.iter().zip(group_tabs) {
                    let row = tab_row(r);
                    self.install_row(&row, tab);
                    self.list.append(&row);
                    row_tabs.push(Some(Rc::downgrade(tab)));
                }
            }
            *self.row_tabs.borrow_mut() = row_tabs;

            while let Some(r) = self.agents_list.first_child() {
                self.agents_list.remove(&r);
            }
            for a in &agents {
                let row = agent_row(a);
                self.agents_list.append(&row);
            }
            if let Some(row) = self.agents_list.row_at_index(0) {
                let (_, natural, _, _) = row.measure(gtk::Orientation::Vertical, -1);
                if natural > 0 && natural != self.agent_row_height.get() {
                    self.agent_row_height.set(natural);
                    self.fit_agents();
                }
            }
            *self.agent_keys.borrow_mut() = agents.iter().map(|a| a.key.clone()).collect();
            let waiting = agents
                .iter()
                .filter(|a| a.status == Some(AgentStatus::NeedsInput))
                .count();
            let working = agents
                .iter()
                .filter(|a| a.status == Some(AgentStatus::Working))
                .count();
            let mut parts = Vec::new();
            if waiting > 0 {
                parts.push(format!("{waiting} waiting"));
            }
            if working > 0 {
                parts.push(format!("{working} working"));
            }
            self.agents_summary.set_text(&parts.join(" · "));
            self.agents_box.set_visible(!agents.is_empty());
            *self.shown.borrow_mut() = (groups, agents, collapsed.to_vec());
        }
        // The selection mirrors the current tab and pane.
        let selected = selected_tab_row(&self.shown.borrow().0, collapsed);
        match selected.and_then(|i| self.list.row_at_index(i as i32)) {
            Some(row) => self.list.select_row(Some(&row)),
            None => self.list.unselect_all(),
        }
        let asel = self.shown.borrow().1.iter().position(|a| a.selected);
        match asel.and_then(|i| self.agents_list.row_at_index(i as i32)) {
            Some(row) => self.agents_list.select_row(Some(&row)),
            None => self.agents_list.unselect_all(),
        }
        let _ = &self.agents_scroll;
    }

    fn install_row(self: &Rc<Self>, row: &gtk::ListBoxRow, tab: &Rc<Tab>) {
        // Right click: the tab's menu.
        let click = gtk::GestureClick::new();
        click.set_button(3);
        let weak = Rc::downgrade(tab);
        let r = row.downgrade();
        click.connect_pressed(move |_, _, x, y| {
            let (Some(tab), Some(row)) = (weak.upgrade(), r.upgrade()) else {
                return;
            };
            app::with_app(|a| a.set_menu_tab(&tab));
            let menu = gio::Menu::new();
            menu.append(Some("Close Tab"), Some("app.close_menu_tab"));
            menu.append(Some("Move Tab to New Workspace"), Some("app.move_menu_tab"));
            let pop = gtk::PopoverMenu::from_model(Some(&menu));
            pop.set_parent(&row);
            pop.set_has_arrow(false);
            pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            pop.connect_closed(|p| {
                let p = p.clone();
                glib::idle_add_local_once(move || p.unparent());
            });
            pop.popup();
        });
        row.add_controller(click);

        // Drag between rows to reorder.
        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::MOVE);
        let weak = Rc::downgrade(tab);
        drag.connect_prepare(move |_, _, _| {
            let tab = weak.upgrade()?;
            let id = app::with_app(|a| a.tab_index(&tab)).flatten()?;
            Some(gdk::ContentProvider::for_value(&(id as u32).to_value()))
        });
        row.add_controller(drag);
        let drop = gtk::DropTarget::new(u32::static_type(), gdk::DragAction::MOVE);
        let weak = Rc::downgrade(tab);
        let r = row.downgrade();
        drop.connect_drop(move |_, value, _, y| {
            let (Some(tab), Some(row)) = (weak.upgrade(), r.upgrade()) else {
                return false;
            };
            let Ok(from) = value.get::<u32>() else {
                return false;
            };
            let after = y > row.height() as f64 / 2.0;
            app::with_app(|a| a.move_tab(from as usize, &tab, after));
            true
        });
        row.add_controller(drop);
    }
}

fn selected_tab_row(groups: &[Group], collapsed: &[String]) -> Option<usize> {
    groups
        .iter()
        .flat_map(|g| {
            let header = g.name.is_some();
            let is_collapsed = g.name.as_ref().is_some_and(|n| collapsed.contains(n));
            // A collapsed group still has a visible header row.
            std::iter::once(false).filter(move |_| header).chain(
                g.rows
                    .iter()
                    .filter(move |_| !is_collapsed)
                    .map(|r| r.selected),
            )
        })
        .position(|s| s)
}

fn dot(status: Option<AgentStatus>) -> gtk::Box {
    let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    d.add_css_class("thurm-dot");
    d.set_valign(gtk::Align::Center);
    d.set_size_request(8, 8);
    if let Some(s) = status {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&format!(
            ".thurm-dot {{ background-color: {}; }}",
            model::status_color(s)
        ));
        #[allow(deprecated)]
        d.style_context()
            .add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    d
}

fn tab_row(r: &TabRow) -> gtk::ListBoxRow {
    let grid = gtk::Grid::new();
    grid.set_column_spacing(8);
    grid.set_margin_start(6);
    grid.set_margin_end(8);
    grid.set_margin_top(4);
    grid.set_margin_bottom(4);
    grid.attach(&dot(r.status), 0, 0, 1, 1);
    let title = gtk::Label::new(Some(&r.title));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.add_css_class("row-title");
    if r.selected {
        title.add_css_class("selected");
    }
    grid.attach(&title, 1, 0, 1, 1);
    let shortcut = gtk::Label::new(Some(&r.shortcut));
    shortcut.add_css_class("row-shortcut");
    grid.attach(&shortcut, 2, 0, 1, 1);
    let subtitle = gtk::Label::new(None);
    subtitle.set_markup(&r.subtitle);
    subtitle.set_xalign(0.0);
    subtitle.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    subtitle.add_css_class("row-subtitle");
    subtitle.set_visible(!r.subtitle.is_empty());
    grid.attach(&subtitle, 1, 1, 2, 1);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&grid));
    row.set_tooltip_text(Some(&r.tooltip));
    row.set_size_request(-1, ROW_HEIGHT);
    row
}

fn agent_row(a: &AgentRow) -> gtk::ListBoxRow {
    let r = TabRow {
        title: a.title.clone(),
        subtitle: glib::markup_escape_text(&a.subtitle).to_string(),
        status: a.status,
        shortcut: String::new(),
        selected: a.selected,
        tooltip: format!("{}\n{}", a.title, a.subtitle),
    };
    let row = tab_row(&r);
    if let Some(grid) = row.child().and_downcast::<gtk::Grid>()
        && let Some(sub) = grid.child_at(1, 1).and_downcast::<gtk::Label>()
    {
        sub.set_ellipsize(gtk::pango::EllipsizeMode::End);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: Option<&str>, selected: &[bool]) -> Group {
        Group {
            name: name.map(str::to_owned),
            rows: selected
                .iter()
                .map(|&selected| TabRow {
                    title: String::new(),
                    subtitle: String::new(),
                    status: None,
                    shortcut: String::new(),
                    selected,
                    tooltip: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn selection_counts_collapsed_headers() {
        let groups = [
            group(Some("registry"), &[false, false]),
            group(Some("hosting"), &[false]),
            group(Some("home-ops"), &[false, false, true]),
        ];
        // Both headers remain visible above the selected tab.
        assert_eq!(
            selected_tab_row(&groups, &["registry".into(), "hosting".into()]),
            Some(5)
        );
        assert_eq!(selected_tab_row(&groups, &["registry".into()]), Some(6));
        assert_eq!(selected_tab_row(&groups, &[]), Some(8));
    }

    #[test]
    fn selection_before_collapsed_group_does_not_move() {
        let groups = [
            group(Some("first"), &[false, true]),
            group(Some("last"), &[false]),
        ];
        assert_eq!(selected_tab_row(&groups, &["last".into()]), Some(2));
    }

    #[test]
    fn hidden_selection_has_no_visible_row() {
        let groups = [
            group(Some("hidden"), &[true]),
            group(Some("visible"), &[false]),
        ];
        assert_eq!(selected_tab_row(&groups, &["hidden".into()]), None);
    }

    #[test]
    fn selection_without_headers() {
        let groups = [group(None, &[false, true])];
        assert_eq!(selected_tab_row(&groups, &[]), Some(1));
        assert_eq!(selected_tab_row(&[], &[]), None);
        assert_eq!(selected_tab_row(&[group(None, &[false])], &[]), None);
    }
}
