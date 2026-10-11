//! One tab: its split tree laid out like TabContentView (SplitView.swift) with 1 px dividers
//! you can drag, zoom, the unfocused-split dim, and the tab's find bar. With pane headers the
//! panes become rounded cards, each under its header.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gtk::prelude::*;
use gtk::{gdk, glib};
use serde_json::{Value, json};
use thurm_proto::layout::TabLayout;

use crate::app;
use crate::config::blend;
use crate::core::{HostId, PaneKey};
use crate::model::{self, Divider, Rect, SplitNode};
use crate::paneheader::{self, PaneHeader};

/// Pixels on each side of a divider that still grab it.
const HIT_SLOP: f64 = 3.0;
/// Corner radius of the split cards.
const CARD_RADIUS: f64 = 8.0;

pub struct Tab {
    /// The tab page's widget.
    pub root: gtk::Overlay,
    container: crate::splitbox::SplitBox,
    pub tree: RefCell<SplitNode>,
    pub focused: RefCell<PaneKey>,
    pub zoomed: RefCell<Option<PaneKey>>,
    /// User title (`thurm tab title`); `None` follows the focused pane.
    pub title: RefCell<Option<String>>,
    pub handoff: RefCell<Option<String>>,
    pub workspace: Cell<u64>,
    pub host: HostId,
    dividers: RefCell<Vec<(gtk::DrawingArea, Divider)>>,
    frames: RefCell<Vec<(PaneKey, Rect)>>,
    headers: RefCell<HashMap<PaneKey, Rc<PaneHeader>>>,
    /// Gutters and card outlines, drawn over the panes; clicks pass through.
    chrome: gtk::DrawingArea,
    /// The cards as last laid out, and whether each is the focused one among several.
    cards: RefCell<Vec<(Rect, bool)>>,
    find: FindBar,
    pub page: RefCell<Option<adw::TabPage>>,
}

struct FindBar {
    revealer: gtk::Revealer,
    entry: gtk::SearchEntry,
    status: gtk::Label,
}

impl Tab {
    pub fn new(tree: SplitNode, focused: PaneKey, workspace: u64) -> Rc<Tab> {
        let host = focused.host.clone();
        let slot: Rc<RefCell<Weak<Tab>>> = Rc::new(RefCell::new(Weak::new()));
        let s2 = slot.clone();
        let container = crate::splitbox::SplitBox::new(move |w, h| {
            if let Some(t) = s2.borrow().upgrade() {
                t.allocate(w, h);
            }
        });
        container.set_hexpand(true);
        container.set_vexpand(true);
        container.add_css_class("thurm-tab");
        let root = gtk::Overlay::new();
        root.set_child(Some(&container));

        let entry = gtk::SearchEntry::new();
        entry.set_placeholder_text(Some("Find"));
        entry.set_width_chars(22);
        let status = gtk::Label::new(None);
        status.add_css_class("dim-label");
        status.add_css_class("caption");
        let prev = gtk::Button::from_icon_name("go-up-symbolic");
        prev.set_tooltip_text(Some("Find Previous (Enter)"));
        prev.add_css_class("flat");
        let next = gtk::Button::from_icon_name("go-down-symbolic");
        next.set_tooltip_text(Some("Find Next (Shift+Enter)"));
        next.add_css_class("flat");
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.set_tooltip_text(Some("Close (Esc)"));
        close.add_css_class("flat");
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bar.add_css_class("thurm-findbar");
        bar.append(&entry);
        bar.append(&status);
        bar.append(&prev);
        bar.append(&next);
        bar.append(&close);
        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::Crossfade);
        revealer.set_child(Some(&bar));
        revealer.set_halign(gtk::Align::End);
        revealer.set_valign(gtk::Align::Start);
        revealer.set_margin_top(8);
        revealer.set_margin_end(12);
        root.add_overlay(&revealer);

        let tab = Rc::new(Tab {
            root,
            container,
            tree: RefCell::new(tree),
            focused: RefCell::new(focused),
            zoomed: RefCell::new(None),
            title: RefCell::new(None),
            handoff: RefCell::new(None),
            workspace: Cell::new(workspace),
            host,
            dividers: RefCell::new(Vec::new()),
            frames: RefCell::new(Vec::new()),
            headers: RefCell::new(HashMap::new()),
            chrome: gtk::DrawingArea::new(),
            cards: RefCell::new(Vec::new()),
            find: FindBar {
                revealer,
                entry,
                status,
            },
            page: RefCell::new(None),
        });

        *slot.borrow_mut() = Rc::downgrade(&tab);
        tab.chrome.set_can_target(false);
        tab.chrome.set_parent(&tab.container);
        let weak = Rc::downgrade(&tab);
        tab.chrome.set_draw_func(move |_, cr, w, h| {
            if let Some(t) = weak.upgrade() {
                t.draw_chrome(cr, w as f64, h as f64);
            }
        });

        // Enter searches older output, Shift+Enter newer; no search while typing.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&tab);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(t) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
                    t.search(if shift { "Forward" } else { "Backward" });
                    glib::Propagation::Stop
                }
                gdk::Key::Escape => {
                    t.close_find();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        tab.find.entry.add_controller(keys);
        let weak = Rc::downgrade(&tab);
        tab.find.entry.connect_stop_search(move |_| {
            if let Some(t) = weak.upgrade() {
                t.close_find();
            }
        });
        let weak = Rc::downgrade(&tab);
        prev.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.search("Backward");
            }
        });
        let weak = Rc::downgrade(&tab);
        next.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.search("Forward");
            }
        });
        let weak = Rc::downgrade(&tab);
        close.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.close_find();
            }
        });
        tab
    }

    pub fn from_layout(l: &TabLayout, host: &str, workspace: u64) -> Option<Rc<Tab>> {
        let tree = SplitNode::from_layout(&l.root, host);
        let panes = tree.panes();
        let focused = panes
            .iter()
            .find(|k| k.id == l.focused)
            .cloned()
            .or_else(|| panes.first().cloned())?;
        let tab = Tab::new(tree, focused, workspace);
        *tab.zoomed.borrow_mut() = l
            .zoomed
            .and_then(|z| panes.iter().find(|k| k.id == z).cloned());
        *tab.title.borrow_mut() = l.title.clone();
        *tab.handoff.borrow_mut() = l.handoff.clone();
        Some(tab)
    }

    pub fn to_layout(&self) -> TabLayout {
        TabLayout {
            title: self.title.borrow().clone(),
            root: self.tree.borrow().to_layout(),
            focused: self.focused.borrow().id,
            zoomed: self.zoomed.borrow().as_ref().map(|k| k.id),
            handoff: self.handoff.borrow().clone(),
        }
    }

    pub fn panes(&self) -> Vec<PaneKey> {
        self.tree.borrow().panes()
    }

    pub fn contains(&self, key: &PaneKey) -> bool {
        self.tree.borrow().contains(key)
    }

    /// Places the panes' widgets in the container (creating views as needed) and drops the
    /// ones no longer in the tree.
    pub fn sync_children(self: &Rc<Self>) {
        let keys = self.panes();
        {
            let mut headers = self.headers.borrow_mut();
            headers.retain(|k, _| keys.contains(k));
            for k in &keys {
                headers.entry(k.clone()).or_insert_with(|| PaneHeader::new(k.clone()));
            }
        }
        let mut wanted: Vec<gtk::Widget> = keys
            .iter()
            .filter_map(|k| app::with_app(|a| a.ensure_view(k).root.clone().upcast()))
            .collect();
        let headers = self.headers.borrow();
        wanted.extend(keys.iter().filter_map(|k| headers.get(k)).map(|h| h.root.clone().upcast()));
        wanted.push(self.chrome.clone().upcast());
        // Remove children that are not wanted (old panes, old dividers).
        let mut child = self.container.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            if !wanted.contains(&c) {
                c.unparent();
            }
        }
        self.dividers.borrow_mut().clear();
        for w in &wanted {
            if w.parent().as_ref() != Some(self.container.upcast_ref()) {
                if w.parent().is_some() {
                    w.unparent();
                }
                w.set_parent(&self.container);
            }
        }
        // Over the panes and headers (dividers go above it).
        self.chrome.insert_before(&self.container, None::<&gtk::Widget>);
        self.container.queue_allocate();
    }

    /// Whether this tab runs the full height of a vertical-tabs window (its headers then take
    /// the window title's place, so even a single pane has one).
    fn full_height(&self) -> bool {
        self.page.borrow().is_some()
            && app::with_app(|a| {
                let ui = a.ui();
                ui.sidebar_tabs() && ui.pane_headers()
            })
            .unwrap_or(false)
    }

    fn draw_chrome(&self, cr: &gtk::cairo::Context, w: f64, h: f64) {
        let cards = self.cards.borrow();
        if cards.is_empty() {
            return;
        }
        let Some((bg, fg, dark, opacity)) = app::with_app(|a| {
            let ui = a.ui();
            (ui.theme.background, ui.theme.foreground, ui.theme_is_dark(), ui.opacity())
        }) else {
            return;
        };
        let rgb = |c: u32| {
            (
                ((c >> 16) & 0xff) as f64 / 255.0,
                ((c >> 8) & 0xff) as f64 / 255.0,
                (c & 0xff) as f64 / 255.0,
            )
        };
        // Gutters in the sidebar's color, around the cards' rounded corners.
        cr.rectangle(0.0, 0.0, w, h);
        for (r, _) in cards.iter() {
            crate::paneicon::rounded(cr, r.x, r.y, r.w, r.h, CARD_RADIUS);
        }
        cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
        let g = rgb(blend(bg, 0x000000, if dark { 0.18 } else { 0.05 }));
        cr.set_source_rgba(g.0, g.1, g.2, opacity);
        let _ = cr.fill();
        cr.set_fill_rule(gtk::cairo::FillRule::Winding);
        cr.set_line_width(1.0);
        for (r, focused) in cards.iter() {
            crate::paneicon::rounded(cr, r.x + 0.5, r.y + 0.5, r.w - 1.0, r.h - 1.0, CARD_RADIUS - 0.5);
            let c = rgb(blend(bg, fg, if *focused { 0.3 } else { 0.12 }));
            cr.set_source_rgba(c.0, c.1, c.2, opacity.max(0.6));
            let _ = cr.stroke();
        }
    }

    fn allocate(self: &Rc<Self>, w: i32, h: i32) {
        let full = Rect::new(0.0, 0.0, w as f64, h as f64);
        let (headers_on, gap) = app::with_app(|a| {
            let ui = a.ui();
            (ui.pane_headers(), ui.pane_gap())
        })
        .unwrap_or((false, 0.0));
        let full_height = self.full_height();
        let zoomed = self.zoomed.borrow().clone().filter(|z| self.contains(z));
        let visible = if zoomed.is_some() { 1 } else { self.panes().len() };
        let cards = headers_on && (visible > 1 || full_height);
        let rect = if cards {
            Rect::new(gap, gap, (full.w - 2.0 * gap).max(1.0), (full.h - 2.0 * gap).max(1.0))
        } else {
            full
        };
        let mut frames = Vec::new();
        let mut dividers = Vec::new();
        match &zoomed {
            Some(z) => frames.push((z.clone(), rect)),
            None => self.tree.borrow().layout(rect, if cards { gap } else { model::GAP }, &mut frames, &mut dividers),
        }
        // Divider widgets: one per divider, on top of the panes.
        {
            let mut have = self.dividers.borrow_mut();
            while have.len() > dividers.len() {
                let (wdg, _) = have.pop().unwrap();
                wdg.unparent();
            }
            while have.len() < dividers.len() {
                let d = self.divider_widget();
                d.set_parent(&self.container);
                let div = dividers[have.len()].clone();
                have.push((d, div));
            }
            for (i, d) in dividers.iter().enumerate() {
                have[i].1 = d.clone();
            }
        }
        let keys = self.panes();
        let focused = self.focused.borrow().clone();
        let multiple = frames.len() > 1;
        let headers = self.headers.borrow();
        let top = frames.iter().map(|(_, r)| r.y).fold(f64::INFINITY, f64::min);
        for k in &keys {
            let Some(view) = app::with_app(|a| a.view(k)).flatten() else { continue };
            let header = headers.get(k);
            match frames.iter().find(|(f, _)| f == k) {
                Some((_, r)) => {
                    view.root.set_child_visible(true);
                    let mut body = *r;
                    if cards && let Some(hd) = header {
                        // Inside the card's 1 px border: the header, then the terminal.
                        let inner = Rect::new(r.x + 1.0, r.y + 1.0, (r.w - 2.0).max(1.0), (r.h - 2.0).max(1.0));
                        let hh = paneheader::HEIGHT as f64;
                        hd.root.set_child_visible(true);
                        set_card_part(hd.root.upcast_ref(), "thurm-card-top");
                        hd.root.size_allocate(
                            &gtk::Allocation::new(inner.x as i32, inner.y as i32, inner.w as i32, hh as i32),
                            -1,
                        );
                        hd.moves_window.set(full_height && (r.y - top).abs() < 1.0);
                        let info = app::with_app(|a| a.infos.borrow().get(k).cloned()).flatten();
                        hd.update(info.as_ref(), *k == focused);
                        body = Rect::new(inner.x, inner.y + hh, inner.w, (inner.h - hh).max(1.0));
                    } else if let Some(hd) = header {
                        hd.root.set_child_visible(false);
                    }
                    set_card_part(view.root.upcast_ref(), if cards { "thurm-card-bottom" } else { "" });
                    view.root.size_allocate(
                        &gtk::Allocation::new(body.x as i32, body.y as i32, body.w.max(1.0) as i32, body.h.max(1.0) as i32),
                        -1,
                    );
                    view.set_dimmed(multiple && *k != focused);
                }
                None => {
                    view.root.set_child_visible(false);
                    if let Some(hd) = header {
                        hd.root.set_child_visible(false);
                    }
                }
            }
        }
        drop(headers);
        *self.cards.borrow_mut() = if cards {
            frames.iter().map(|(k, r)| (*r, multiple && *k == focused)).collect()
        } else {
            Vec::new()
        };
        self.chrome.set_child_visible(cards);
        self.chrome.size_allocate(&gtk::Allocation::new(0, 0, w.max(1), h.max(1)), -1);
        self.chrome.queue_draw();
        if full_height {
            // The window controls go into the top corner panes' headers.
            let in_row: Vec<&(PaneKey, Rect)> = frames.iter().filter(|(_, r)| (r.y - top).abs() < 1.0).collect();
            let lead = in_row.iter().min_by(|a, b| a.1.x.total_cmp(&b.1.x)).map(|(k, _)| k.clone());
            let trail = in_row.iter().max_by(|a, b| a.1.max_x().total_cmp(&b.1.max_x())).map(|(k, _)| k.clone());
            let weak = Rc::downgrade(self);
            app::with_app(|a| {
                if let Some(win) = a.win() {
                    win.place_controls_later(weak, lead, trail);
                }
            });
        }
        for (wdg, d) in self.dividers.borrow().iter() {
            let r = match d.axis {
                model::Axis::Horizontal => Rect::new(d.line.x - HIT_SLOP, d.line.y, d.line.w + 2.0 * HIT_SLOP, d.line.h),
                model::Axis::Vertical => Rect::new(d.line.x, d.line.y - HIT_SLOP, d.line.w, d.line.h + 2.0 * HIT_SLOP),
            };
            wdg.set_cursor_from_name(Some(match d.axis {
                model::Axis::Horizontal => "col-resize",
                model::Axis::Vertical => "row-resize",
            }));
            wdg.size_allocate(
                &gtk::Allocation::new(r.x as i32, r.y as i32, r.w.max(1.0) as i32, r.h.max(1.0) as i32),
                -1,
            );
        }
        *self.frames.borrow_mut() = frames;
    }

    fn divider_widget(self: &Rc<Self>) -> gtk::DrawingArea {
        let d = gtk::DrawingArea::new();
        let tab = Rc::downgrade(self);
        d.set_draw_func(move |area, cr, w, h| {
            // Cards have gutters instead of lines.
            if tab.upgrade().is_some_and(|t| !t.cards.borrow().is_empty()) {
                return;
            }
            let color = app::with_app(|a| a.divider_color()).unwrap_or(0x444444);
            cr.set_source_rgb(
                ((color >> 16) & 0xff) as f64 / 255.0,
                ((color >> 8) & 0xff) as f64 / 255.0,
                (color & 0xff) as f64 / 255.0,
            );
            if w > h {
                cr.rectangle(0.0, HIT_SLOP, w as f64, 1.0);
            } else {
                cr.rectangle(HIT_SLOP, 0.0, 1.0, h as f64);
            }
            let _ = cr.fill();
            let _ = area;
        });
        let drag = gtk::GestureDrag::new();
        let weak: Weak<Tab> = Rc::downgrade(self);
        let start: Rc<Cell<(f64, f64)>> = Rc::new(Cell::new((0.0, 0.0)));
        let s2 = start.clone();
        let dw = d.downgrade();
        let tw = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, y| {
            if let (Some(d), Some(t)) = (dw.upgrade(), tw.upgrade())
                && let Some(b) = d.compute_bounds(&t.container)
            {
                s2.set((b.x() as f64 + x, b.y() as f64 + y));
            }
        });
        let dw = d.downgrade();
        drag.connect_drag_update(move |_, ox, oy| {
            let (Some(t), Some(d)) = (weak.upgrade(), dw.upgrade()) else { return };
            let (sx, sy) = start.get();
            let (px, py) = (sx + ox, sy + oy);
            let found = t
                .dividers
                .borrow()
                .iter()
                .find(|(w, _)| *w == d)
                .map(|(_, div)| div.clone());
            if let Some(div) = found {
                let c = div.container;
                let ratio = match div.axis {
                    model::Axis::Horizontal => (px - c.x) / c.w.max(1.0),
                    model::Axis::Vertical => (py - c.y) / c.h.max(1.0),
                };
                t.tree.borrow_mut().set_ratio(&div.path, ratio);
                t.container.queue_allocate();
            }
        });
        drag.connect_drag_end(|_, _, _| {
            app::with_app(|a| a.schedule_save());
        });
        d.add_controller(drag);
        d
    }

    pub fn relayout(&self) {
        self.container.queue_allocate();
    }

    /// The header of `key` (for the window controls).
    pub fn header(&self, key: &PaneKey) -> Option<Rc<PaneHeader>> {
        self.headers.borrow().get(key).cloned()
    }

    /// Re-reads the panes' titles and icons into their headers.
    pub fn refresh_headers(&self) {
        if self.cards.borrow().is_empty() {
            return;
        }
        let focused = self.focused.borrow().clone();
        for (k, h) in self.headers.borrow().iter() {
            let info = app::with_app(|a| a.infos.borrow().get(k).cloned()).flatten();
            h.update(info.as_ref(), *k == focused);
        }
    }

    /// Pane frames as last laid out (directional focus, nearest pane).
    pub fn frames(&self) -> Vec<(PaneKey, Rect)> {
        self.frames.borrow().clone()
    }

    pub fn set_focused(&self, key: &PaneKey) {
        if self.zoomed.borrow().as_ref().is_some_and(|z| z != key) {
            *self.zoomed.borrow_mut() = None;
        }
        *self.focused.borrow_mut() = key.clone();
        self.relayout();
    }

    // MARK: find

    pub fn show_find(&self) {
        self.find.revealer.set_reveal_child(true);
        self.find.entry.grab_focus();
        self.find.entry.select_region(0, -1);
    }

    pub fn find_visible(&self) -> bool {
        self.find.revealer.reveals_child()
    }

    /// `"Backward"` searches older output.
    pub fn search(&self, direction: &str) {
        let query = self.find.entry.text().to_string();
        if query.is_empty() {
            return;
        }
        let key = self.focused.borrow().clone();
        let resp = app::with_app(|a| {
            a.core(&key.host).map(|c| {
                c.request(&json!({"Search": {"pane": key.id, "query": query, "direction": direction}}))
            })
        })
        .flatten()
        .unwrap_or(Value::Null);
        let found = resp
            .pointer("/Search/found")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.find.status.set_text(if found { "" } else { "No matches" });
        app::with_app(|a| {
            if let Some(v) = a.view(&key) {
                v.area.queue_draw();
            }
        });
    }

    pub fn close_find(&self) {
        let key = self.focused.borrow().clone();
        app::with_app(|a| {
            if let Some(c) = a.core(&key.host) {
                c.send(&json!({"Search": {"pane": key.id, "query": null, "direction": "Backward"}}));
            }
            if let Some(v) = a.view(&key) {
                v.area.queue_draw();
                v.grab_focus();
            }
        });
        self.find.status.set_text("");
        self.find.revealer.set_reveal_child(false);
    }
}

/// Rounds a card's header (`thurm-card-top`) or terminal (`thurm-card-bottom`) and clips its
/// content to the corners, so nothing shows through a translucent gutter there; `""` squares it.
fn set_card_part(w: &gtk::Widget, part: &str) {
    for c in ["thurm-card-top", "thurm-card-bottom"] {
        if c == part {
            w.add_css_class(c);
        } else {
            w.remove_css_class(c);
        }
    }
    w.set_overflow(if part.is_empty() { gtk::Overflow::Visible } else { gtk::Overflow::Hidden });
}
