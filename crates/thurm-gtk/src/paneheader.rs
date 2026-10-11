//! The strip above a split pane (PaneHeader.swift, `window.pane_headers`): what runs in it, and
//! buttons to split, zoom or close it. Clicking it focuses the pane; double-clicking zooms it.
//! Under a full-height window (vertical tabs) it also moves the window, and the corner panes'
//! headers hold the window controls.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{cairo, gdk};
use thurm_proto::{AgentStatus, PaneInfo};

use crate::app;
use crate::core::PaneKey;
use crate::model;
use crate::paneicon::PaneIcon;

pub const HEIGHT: i32 = 30;

pub struct PaneHeader {
    pub key: PaneKey,
    pub root: gtk::Box,
    /// Holds the window's leading controls when this is the top-left pane (sidebar hidden).
    pub lead: gtk::Box,
    /// Holds the window controls when this is the top-right pane.
    pub trail: gtk::Box,
    icon: gtk::DrawingArea,
    dot: gtk::DrawingArea,
    title: gtk::Label,
    kind: Cell<PaneIcon>,
    status: Cell<Option<AgentStatus>>,
    /// Dragging the header moves the window (full-height windows).
    pub moves_window: Cell<bool>,
    /// The title and tooltip as last set.
    shown: RefCell<(String, String)>,
}

#[derive(Clone, Copy)]
enum Button {
    SplitRight,
    SplitDown,
    Zoom,
    Close,
}

impl PaneHeader {
    pub fn new(key: PaneKey) -> Rc<PaneHeader> {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("thurm-pane-header");
        root.set_size_request(-1, HEIGHT);

        let lead = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        lead.set_valign(gtk::Align::Center);
        lead.set_margin_start(4);
        root.append(&lead);

        let icon = gtk::DrawingArea::new();
        icon.set_content_width(15);
        icon.set_content_height(15);
        icon.add_css_class("thurm-pane-icon");
        let dot = gtk::DrawingArea::new();
        dot.set_content_width(7);
        dot.set_content_height(7);
        dot.set_halign(gtk::Align::End);
        dot.set_valign(gtk::Align::End);
        dot.set_can_target(false);
        let icon_box = gtk::Overlay::new();
        icon_box.set_child(Some(&icon));
        icon_box.add_overlay(&dot);
        icon_box.set_valign(gtk::Align::Center);
        icon_box.set_margin_start(6);
        icon_box.set_margin_end(7);
        root.append(&icon_box);

        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.add_css_class("thurm-pane-title");
        root.append(&title);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        buttons.set_valign(gtk::Align::Center);
        buttons.set_margin_end(6);
        root.append(&buttons);

        let trail = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        trail.set_valign(gtk::Align::Center);
        root.append(&trail);

        let header = Rc::new(PaneHeader {
            key,
            root,
            lead,
            trail,
            icon,
            dot,
            title,
            kind: Cell::new(PaneIcon::Shell),
            status: Cell::new(None),
            moves_window: Cell::new(false),
            shown: RefCell::new((String::new(), String::new())),
        });

        for (b, tip) in [
            (Button::SplitRight, "Split Right"),
            (Button::SplitDown, "Split Down"),
            (Button::Zoom, "Zoom"),
            (Button::Close, "Close Pane"),
        ] {
            let art = gtk::DrawingArea::new();
            art.set_content_width(14);
            art.set_content_height(14);
            art.set_draw_func(move |area, cr, w, h| {
                let c = area.color();
                cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64);
                draw_button(cr, b, w as f64, h as f64);
            });
            let button = gtk::Button::new();
            button.set_child(Some(&art));
            button.add_css_class("flat");
            button.add_css_class("thurm-pane-button");
            button.set_tooltip_text(Some(tip));
            button.set_focusable(false);
            let key = header.key.clone();
            button.connect_clicked(move |_| {
                app::with_app(|a| a.pane_header_action(&key, button_action(b)));
            });
            buttons.append(&button);
        }

        let weak = Rc::downgrade(&header);
        header.icon.set_draw_func(move |area, cr, w, _| {
            let Some(h) = weak.upgrade() else { return };
            let c = area.color();
            cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64);
            h.kind.get().draw_glyph(cr, w as f64);
        });
        let weak = Rc::downgrade(&header);
        header.dot.set_draw_func(move |_, cr, w, h| {
            let Some(hd) = weak.upgrade() else { return };
            let Some(s) = hd.status.get() else { return };
            let rgb = parse_hex(model::status_color(s));
            cr.arc(w as f64 / 2.0, h as f64 / 2.0, w.min(h) as f64 / 2.0, 0.0, std::f64::consts::TAU);
            cr.set_source_rgb(rgb.0, rgb.1, rgb.2);
            let _ = cr.fill();
        });

        // Click: focus the pane; double-click: zoom it.
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        let key = header.key.clone();
        let root = header.root.clone();
        click.connect_pressed(move |g, n, x, y| {
            if on_button(&root, x, y) {
                return;
            }
            g.set_state(gtk::EventSequenceState::Claimed);
            app::with_app(|a| {
                a.focus_pane(&key);
                if n == 2 {
                    a.pane_header_action(&key, "zoom_split");
                }
            });
        });
        header.root.add_controller(click);

        // Dragging moves a full-height window, like a header bar.
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        let weak = Rc::downgrade(&header);
        let moved = Rc::new(Cell::new(false));
        let m2 = moved.clone();
        drag.connect_drag_begin(move |_, _, _| m2.set(false));
        drag.connect_drag_update(move |g, ox, oy| {
            let Some(h) = weak.upgrade() else { return };
            if moved.get() || !h.moves_window.get() || ox.hypot(oy) < 4.0 {
                return;
            }
            let Some((x, y)) = g.start_point() else { return };
            if on_button(&h.root, x, y) {
                return;
            }
            moved.set(true);
            begin_move(h.root.upcast_ref(), g, x, y);
        });
        header.root.add_controller(drag);
        header
    }

    /// Refreshes the icon, title and status dot from the pane's info.
    pub fn update(&self, info: Option<&PaneInfo>, focused: bool) {
        if focused {
            self.root.add_css_class("focused");
        } else {
            self.root.remove_css_class("focused");
        }
        let kind = PaneIcon::of(info);
        if kind != self.kind.get() {
            self.kind.set(kind);
            self.icon.queue_draw();
        }
        let status = info.and_then(|i| i.agent.as_ref()).map(|a| a.status).filter(|s| *s != AgentStatus::Idle);
        if status != self.status.get() {
            self.status.set(status);
            self.dot.queue_draw();
        }
        let mut text = model::display_title(info);
        if let Some(a) = info.and_then(|i| i.agent.as_ref())
            && (text.is_empty() || text == "Thurm")
        {
            text = a.name.clone();
        }
        if self.key.is_remote() {
            text = format!("{} · {text}", self.key.host);
        }
        let tip = match info.and_then(|i| i.cwd.as_deref()) {
            Some(cwd) => format!("{text}\n{cwd}"),
            None => text.clone(),
        };
        let mut shown = self.shown.borrow_mut();
        if shown.0 != text {
            self.title.set_text(&text);
            shown.0 = text;
        }
        if shown.1 != tip {
            self.root.set_tooltip_text(Some(&tip));
            shown.1 = tip;
        }
    }
}

fn button_action(b: Button) -> &'static str {
    match b {
        Button::SplitRight => "split_right",
        Button::SplitDown => "split_down",
        Button::Zoom => "zoom_split",
        Button::Close => "close_pane",
    }
}

/// Whether (`x`, `y`) in `root` hits one of its buttons (they handle their own clicks).
fn on_button(root: &gtk::Box, x: f64, y: f64) -> bool {
    let mut w = root.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(v) = w {
        if v.is::<gtk::Button>() {
            return true;
        }
        if v == *root.upcast_ref::<gtk::Widget>() {
            return false;
        }
        w = v.parent();
    }
    false
}

/// Hands the pointer to the compositor to move the window (what GtkWindowHandle does).
fn begin_move(widget: &gtk::Widget, g: &gtk::GestureDrag, x: f64, y: f64) {
    let Some(native) = widget.native() else { return };
    let Some(toplevel) = native.surface().and_then(|s| s.downcast::<gdk::Toplevel>().ok()) else {
        return;
    };
    let Some(p) = widget.compute_point(&native, &gtk::graphene::Point::new(x as f32, y as f32)) else {
        return;
    };
    let (sx, sy) = native.surface_transform();
    let Some(device) = g.device() else { return };
    toplevel.begin_move(
        &device,
        g.current_button() as i32,
        p.x() as f64 + sx,
        p.y() as f64 + sy,
        g.current_event_time(),
    );
}

/// The button's glyph, 12 px, centered in `w`×`h`.
fn draw_button(cr: &cairo::Context, b: Button, w: f64, h: f64) {
    const S: f64 = 12.0;
    cr.translate(((w - S) / 2.0).round(), ((h - S) / 2.0).round());
    cr.set_line_width(1.1);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    match b {
        Button::SplitRight | Button::SplitDown => {
            crate::paneicon::rounded(cr, 0.5, 1.5, S - 1.0, S - 3.0, 2.0);
            if matches!(b, Button::SplitRight) {
                cr.move_to(S / 2.0, 1.5);
                cr.line_to(S / 2.0, S - 1.5);
            } else {
                cr.move_to(0.5, S / 2.0);
                cr.line_to(S - 0.5, S / 2.0);
            }
        }
        Button::Zoom => {
            // Arrows out to the top-left and bottom-right corners.
            let (a, z, k) = (1.0, S - 1.0, 3.5);
            cr.move_to(a, a + k);
            cr.line_to(a, a);
            cr.line_to(a + k, a);
            cr.move_to(a, a);
            cr.line_to(a + 4.0, a + 4.0);
            cr.move_to(z, z - k);
            cr.line_to(z, z);
            cr.line_to(z - k, z);
            cr.move_to(z, z);
            cr.line_to(z - 4.0, z - 4.0);
        }
        Button::Close => {
            let (a, z) = (2.0, S - 2.0);
            cr.move_to(a, a);
            cr.line_to(z, z);
            cr.move_to(z, a);
            cr.line_to(a, z);
        }
    }
    let _ = cr.stroke();
}

fn parse_hex(c: &str) -> (f64, f64, f64) {
    let v = u32::from_str_radix(c.trim_start_matches('#'), 16).unwrap_or(0x888888);
    (
        ((v >> 16) & 0xff) as f64 / 255.0,
        ((v >> 8) & 0xff) as f64 / 255.0,
        (v & 0xff) as f64 / 255.0,
    )
}
