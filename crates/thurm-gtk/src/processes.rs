//! Processes & Ports (ProcessPanel.swift): every pane's processes and the ports they listen
//! on, refreshed every 2 s.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use serde_json::json;
use thurm_proto::PaneProcesses;

use crate::app::{self, App};
use crate::core::{LOCAL, PaneKey};

struct Panel {
    window: adw::Window,
    list: gtk::ListBox,
    ports_only: gtk::CheckButton,
    /// Rows per host, in connection order.
    rows: RefCell<Vec<(String, Vec<PaneProcesses>)>>,
    pending: RefCell<Vec<String>>,
    keys: RefCell<Vec<(PaneKey, Option<u16>)>>,
}

thread_local! {
    static PANEL: RefCell<Option<Rc<Panel>>> = const { RefCell::new(None) };
}

pub fn show(app: &Rc<App>) {
    if let Some(p) = PANEL.with(|p| p.borrow().clone()) {
        p.window.present();
        return;
    }
    let window = adw::Window::new();
    window.set_title(Some("Processes & Ports"));
    window.set_default_size(720, 380);
    if let Some(w) = app.win() {
        window.set_transient_for(Some(&w.window));
    }
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    // A click focuses the pane; opening a port takes a double click (or Enter), as on macOS.
    list.set_activate_on_single_click(false);
    list.add_css_class("rich-list");
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    let ports_only = gtk::CheckButton::with_label("Only processes with ports");
    ports_only.set_margin_start(12);
    ports_only.set_margin_top(6);
    ports_only.set_margin_bottom(6);
    let header = header_row(&["Pane", "PID", "Ports", "Command"], true);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&header);
    body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    body.append(&scroll);
    body.append(&ports_only);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&body));
    window.set_content(Some(&toolbar));

    let panel = Rc::new(Panel {
        window: window.clone(),
        list,
        ports_only,
        rows: RefCell::new(Vec::new()),
        pending: RefCell::new(Vec::new()),
        keys: RefCell::new(Vec::new()),
    });
    PANEL.with(|p| *p.borrow_mut() = Some(panel.clone()));

    let weak = Rc::downgrade(&panel);
    panel.ports_only.connect_toggled(move |_| {
        if let Some(p) = weak.upgrade() {
            p.render();
        }
    });
    // A click on a row brings its pane forward (like the macOS panel); moving the selection
    // with the keyboard or a refresh does not.
    let weak = Rc::downgrade(&panel);
    let click = gtk::GestureClick::new();
    click.connect_released(move |_, n, _, y| {
        let Some(p) = weak.upgrade() else { return };
        let Some(row) = p.list.row_at_y(y as i32).filter(|_| n == 1) else {
            return;
        };
        let key = p
            .keys
            .borrow()
            .get(row.index() as usize)
            .map(|(k, _)| k.clone());
        if let Some(k) = key {
            app::with_app(|a| a.focus_agent(&k));
            p.window.present();
        }
    });
    panel.list.add_controller(click);
    let weak = Rc::downgrade(&panel);
    panel.list.connect_row_activated(move |_, row| {
        let Some(p) = weak.upgrade() else { return };
        let port = p
            .keys
            .borrow()
            .get(row.index() as usize)
            .and_then(|(k, port)| (!k.is_remote()).then_some(*port).flatten());
        if let Some(port) = port {
            let _ = gio::AppInfo::launch_default_for_uri(
                &format!("http://localhost:{port}"),
                None::<&gio::AppLaunchContext>,
            );
        }
    });
    // Esc closes it, like the macOS panels.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let w = window.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            w.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
    window.connect_close_request(|_| {
        PANEL.with(|p| p.borrow_mut().take());
        glib::Propagation::Proceed
    });
    refresh(app);
    let weak = Rc::downgrade(&panel);
    glib::timeout_add_local(Duration::from_secs(2), move || {
        if weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        app::with_app(refresh);
        glib::ControlFlow::Continue
    });
    window.present();
}

fn refresh(app: &Rc<App>) {
    let Some(panel) = PANEL.with(|p| p.borrow().clone()) else {
        return;
    };
    let mut hosts = vec![LOCAL.to_string()];
    hosts.extend(app.connected_remotes());
    panel.rows.borrow_mut().retain(|(h, _)| hosts.contains(h));
    for host in hosts {
        if panel.pending.borrow().contains(&host) {
            continue;
        }
        let Some(core) = app.core(&host) else {
            continue;
        };
        panel.pending.borrow_mut().push(host.clone());
        let weak = Rc::downgrade(&panel);
        core.request_async(&json!({"Processes": {"pane": null}}), 5000, move |resp| {
            let Some(p) = weak.upgrade() else { return };
            p.pending.borrow_mut().retain(|h| *h != host);
            let procs: Vec<PaneProcesses> = resp
                .get("Processes")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            {
                let mut rows = p.rows.borrow_mut();
                match rows.iter_mut().find(|(h, _)| *h == host) {
                    Some(r) => r.1 = procs,
                    None => {
                        if host == LOCAL {
                            rows.insert(0, (host.clone(), procs));
                        } else {
                            rows.push((host.clone(), procs));
                        }
                    }
                }
            }
            p.render();
        });
    }
}

fn header_row(cols: &[&str], header: bool) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.set_margin_start(12);
    b.set_margin_end(12);
    b.set_margin_top(4);
    b.set_margin_bottom(4);
    for (i, c) in cols.iter().enumerate() {
        let l = gtk::Label::new(Some(c));
        l.set_xalign(0.0);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l.set_width_chars([6, 8, 12, 0][i.min(3)]);
        l.set_max_width_chars([6, 8, 12, 80][i.min(3)]);
        if i == 3 {
            l.set_hexpand(true);
        }
        if header {
            l.add_css_class("heading");
        } else {
            l.add_css_class("monospace");
        }
        b.append(&l);
    }
    b
}

impl Panel {
    fn render(&self) {
        while let Some(r) = self.list.first_child() {
            self.list.remove(&r);
        }
        let only_ports = self.ports_only.is_active();
        let mut keys = Vec::new();
        for (host, panes) in self.rows.borrow().iter() {
            for pp in panes {
                let key = PaneKey::new(host, pp.pane);
                let mut depth: HashMap<u32, usize> = HashMap::new();
                for proc in &pp.processes {
                    let d = depth.get(&proc.ppid).map_or(0, |d| d + 1);
                    depth.insert(proc.pid, d);
                    if only_ports && proc.ports.is_empty() {
                        continue;
                    }
                    let ports = proc
                        .ports
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let command = format!("{}{}", "  ".repeat(d.min(8)), proc.command);
                    let row = header_row(
                        &[&key.to_string(), &proc.pid.to_string(), &ports, &command],
                        false,
                    );
                    if !ports.is_empty()
                        && let Some(l) = row
                            .first_child()
                            .and_then(|c| c.next_sibling())
                            .and_then(|c| c.next_sibling())
                    {
                        l.add_css_class("success");
                    }
                    self.list.append(&row);
                    keys.push((key.clone(), proc.ports.first().copied()));
                }
            }
        }
        *self.keys.borrow_mut() = keys;
    }
}
