//! The picker behind the command palette, workspace switcher, agent picker and theme browser
//! (CommandPalette.swift): a search field over a fuzzy-filtered list, shown over the window.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::model;
use crate::window::MainWindow;

pub type Callback = Rc<dyn Fn()>;

pub struct Item {
    pub title: String,
    pub detail: String,
    pub shortcut: String,
    pub action: Callback,
    pub preview: Option<Callback>,
    pub rename: Option<Callback>,
}

impl Item {
    pub fn new(
        title: impl Into<String>,
        detail: impl Into<String>,
        action: impl Fn() + 'static,
    ) -> Item {
        Item {
            title: title.into(),
            detail: detail.into(),
            shortcut: String::new(),
            action: Rc::new(action),
            preview: None,
            rename: None,
        }
    }

    pub fn shortcut(mut self, s: impl Into<String>) -> Item {
        self.shortcut = s.into();
        self
    }
}

struct Open {
    root: gtk::Box,
    overlay: gtk::Overlay,
    items: Vec<Item>,
    shown: Vec<usize>,
    on_cancel: Option<Callback>,
    previewed: Option<String>,
    target: glib::WeakRef<gtk::Widget>,
}

thread_local! {
    static OPEN: RefCell<Option<Open>> = const { RefCell::new(None) };
}

/// Shows the picker; `initial` is the row selected first (its preview is not run).
pub fn show(
    win: &MainWindow,
    items: Vec<Item>,
    placeholder: &str,
    footer: Option<&str>,
    initial: usize,
    on_cancel: Option<Callback>,
) {
    close(true);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("thurm-palette");
    root.set_halign(gtk::Align::Center);
    root.set_valign(gtk::Align::Start);
    root.set_margin_top(60);
    root.set_size_request(560, 340);

    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some(placeholder));
    root.append(&entry);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_activate_on_single_click(false);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    root.append(&scroll);
    if let Some(f) = footer {
        let l = gtk::Label::new(Some(f));
        l.set_xalign(0.0);
        l.add_css_class("footer");
        root.append(&l);
    }
    win.overlay.add_overlay(&root);

    let target = GtkWindowExt::focus(&win.window)
        .map(|w| w.downgrade())
        .unwrap_or_default();
    let previewed = items.get(initial).map(|i| i.title.clone());
    OPEN.with(|o| {
        *o.borrow_mut() = Some(Open {
            root: root.clone(),
            overlay: win.overlay.clone(),
            items,
            shown: Vec::new(),
            on_cancel,
            previewed,
            target,
        })
    });
    refilter(&list, "", initial);

    let l = list.clone();
    entry.connect_changed(move |e| refilter(&l, &e.text(), 0));
    let l = list.clone();
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(move |_, key, _, state| {
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::Down | gdk::Key::Up => {
                let i = l.selected_row().map_or(0, |r| r.index());
                let n = OPEN.with(|o| o.borrow().as_ref().map_or(0, |o| o.shown.len())) as i32;
                let next = if key == gdk::Key::Down {
                    (i + 1).min(n - 1)
                } else {
                    (i - 1).max(0)
                };
                select(&l, next);
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                run_selected(&l);
                glib::Propagation::Stop
            }
            gdk::Key::Escape => {
                close(true);
                glib::Propagation::Stop
            }
            gdk::Key::r if ctrl => {
                let i = l.selected_row().map_or(-1, |r| r.index());
                let rename = OPEN.with(|o| {
                    o.borrow().as_ref().and_then(|o| {
                        o.shown
                            .get(i as usize)
                            .and_then(|&j| o.items[j].rename.clone())
                    })
                });
                if let Some(r) = rename {
                    close(false);
                    r();
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    entry.add_controller(keys);
    list.connect_row_activated(|l, _| run_selected(l));
    list.connect_row_selected(|_, row| {
        let Some(row) = row else { return };
        let preview = OPEN.with(|o| {
            let mut o = o.borrow_mut();
            let o = o.as_mut()?;
            let &j = o.shown.get(row.index() as usize)?;
            let item = &o.items[j];
            if o.previewed.as_deref() == Some(item.title.as_str()) {
                return None;
            }
            o.previewed = Some(item.title.clone());
            item.preview.clone()
        });
        if let Some(p) = preview {
            p();
        }
    });
    // Clicking elsewhere closes it, like the panel losing key.
    let focus = gtk::EventControllerFocus::new();
    focus.connect_leave(|_| {
        glib::idle_add_local_once(|| {
            let inside = OPEN.with(|o| {
                o.borrow().as_ref().is_some_and(|o| {
                    o.root
                        .root()
                        .and_then(|r| r.focus())
                        .is_some_and(|f| f.is_ancestor(&o.root))
                })
            });
            if !inside {
                close(true);
            }
        });
    });
    root.add_controller(focus);
    entry.grab_focus();
}

fn refilter(list: &gtk::ListBox, query: &str, select_row: usize) {
    while let Some(r) = list.first_child() {
        list.remove(&r);
    }
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let Some(o) = o.as_mut() else { return };
        let mut scored: Vec<(usize, usize)> = o
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| model::fuzzy_score(&it.title, query).map(|s| (s, i)))
            .collect();
        scored.sort_by_key(|(s, i)| (*s, *i));
        o.shown = scored.into_iter().map(|(_, i)| i).collect();
        for &i in &o.shown {
            let it = &o.items[i];
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            b.set_margin_start(10);
            b.set_margin_end(10);
            b.set_margin_top(4);
            b.set_margin_bottom(4);
            let t = gtk::Label::new(Some(&it.title));
            t.set_xalign(0.0);
            t.set_ellipsize(gtk::pango::EllipsizeMode::End);
            b.append(&t);
            let d = gtk::Label::new(Some(&it.detail));
            d.set_xalign(0.0);
            d.set_hexpand(true);
            d.set_ellipsize(gtk::pango::EllipsizeMode::End);
            d.add_css_class("detail");
            d.add_css_class("dim-label");
            b.append(&d);
            if !it.shortcut.is_empty() {
                let s = gtk::Label::new(Some(&it.shortcut));
                s.add_css_class("detail");
                s.add_css_class("dim-label");
                b.append(&s);
            }
            list.append(&b);
        }
    });
    select(list, select_row as i32);
}

fn select(list: &gtk::ListBox, i: i32) {
    if let Some(row) = list.row_at_index(i.max(0)) {
        list.select_row(Some(&row));
        row.grab_focus();
        // Typing goes on in the field.
        if let Some(entry) = list
            .ancestor(gtk::Box::static_type())
            .and_then(|b| b.first_child())
            .and_downcast::<gtk::Entry>()
        {
            entry.grab_focus_without_selecting();
        }
    }
}

fn run_selected(list: &gtk::ListBox) {
    let i = list.selected_row().map_or(-1, |r| r.index());
    let action = OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let o = o.as_mut()?;
        let &j = o.shown.get(i as usize)?;
        o.on_cancel = None;
        Some(o.items[j].action.clone())
    });
    if let Some(a) = action {
        close(false);
        a();
    }
}

/// Closes the picker; `cancelled` runs its cancel handler (a theme preview goes back).
pub fn close(cancelled: bool) {
    let open = OPEN.with(|o| o.borrow_mut().take());
    let Some(open) = open else { return };
    open.overlay.remove_overlay(&open.root);
    if let Some(t) = open.target.upgrade() {
        t.grab_focus();
    }
    if cancelled && let Some(c) = open.on_cancel {
        c();
    }
}
