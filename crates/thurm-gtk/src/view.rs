//! One pane on screen (TerminalView.swift): draws the grid, forwards keyboard (through the input
//! method), mouse, wheel, focus and size changes, and hosts the pane's overlays: progress bar,
//! toast, completion popup, offline notice and lock badge.

use std::cell::{Cell as StdCell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::{Value, json};
use thurm_proto::{CompletionItem, Completions, PaneInfo, ProgressState};

use crate::app;
use crate::core::{Core, PaneKey};
use crate::keys;
use crate::model;
use crate::render::{self, Fonts, Hover, ImageCache, PaintParams, Snapshot};

const MOUSE_PRESS: u8 = 0;
const MOUSE_RELEASE: u8 = 1;
const MOUSE_MOVE: u8 = 2;
const BUTTON_LEFT: u8 = 0;
const BUTTON_MIDDLE: u8 = 1;
const BUTTON_RIGHT: u8 = 2;
const BUTTON_BACK: u8 = 3;
const BUTTON_FORWARD: u8 = 4;
const BUTTON_NONE: u8 = 5;

const BLINK: f64 = 0.53;
const FLASH: f64 = 0.25;

#[derive(Default)]
struct State {
    /// Size last sent to the daemon: cols, rows, cell w, cell h.
    sent: Option<(u16, u16, u16, u16)>,
    subscribed: bool,
    reported_focus: Option<bool>,
    snapshot: Snapshot,
    images: ImageCache,
    /// The input method is composing; its preedit text.
    preedit: String,
    composing: bool,
    /// Text the input method committed during the current key press.
    committed: Option<String>,
    scroll_acc: f64,
    pressed: Option<u8>,
    clicks: u8,
    down: std::collections::HashSet<u32>,
    /// Keys whose press reached the terminal: their release does too.
    delivered: std::collections::HashSet<u32>,
    blink_epoch: Option<Instant>,
    flash_start: Option<Instant>,
    // Smooth scrolling.
    smooth_active: bool,
    smooth_pos: f64,
    sent_offset: u32,
    last_smooth_input: Option<Instant>,
    // Links.
    hover: Hover,
    pointer: Option<(f64, f64)>,
    link_mod: bool,
    // Completion.
    completion: Option<Completion>,
    completion_query: u64,
    restored_toast_shown: bool,
    offline: Option<String>,
    dimmed: bool,
    // Accessibility.
    last_rows: Vec<String>,
    last_key: Option<Instant>,
    announce_queue: VecDeque<String>,
    announce_scheduled: bool,
}

struct Completion {
    word: String,
    items: Vec<CompletionItem>,
    selected: usize,
}

pub struct TermView {
    pub key: PaneKey,
    /// The pane's top widget (goes into the split container).
    pub root: gtk::Overlay,
    pub area: gtk::DrawingArea,
    /// The same widget, as the screen reader's text.
    text: crate::termarea::TermArea,
    fonts: RefCell<Rc<Fonts>>,
    state: RefCell<State>,
    im: gtk::IMMulticontext,
    focused: StdCell<bool>,
    progress: gtk::DrawingArea,
    progress_state: RefCell<Option<thurm_proto::Progress>>,
    progress_started: StdCell<Option<Instant>>,
    toast: gtk::Revealer,
    toast_label: gtk::Label,
    toast_generation: StdCell<u64>,
    offline_box: gtk::Box,
    offline_label: gtk::Label,
    lock: gtk::Image,
    popup: gtk::Box,
    popup_list: gtk::ListBox,
    tick: RefCell<Option<gtk::TickCallbackId>>,
    blink_timer: RefCell<Option<glib::SourceId>>,
}

impl TermView {
    pub fn new(key: PaneKey, fonts: Rc<Fonts>) -> Rc<TermView> {
        let text = crate::termarea::TermArea::default();
        let area: gtk::DrawingArea = text.clone().upcast();
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.set_focusable(true);
        area.set_can_focus(true);
        area.set_cursor_from_name(Some("text"));
        area.set_accessible_role(gtk::AccessibleRole::Terminal);
        area.update_property(&[gtk::accessible::Property::Label("Terminal")]);

        let root = gtk::Overlay::new();
        root.set_child(Some(&area));
        root.add_css_class("thurm-pane");

        let progress = gtk::DrawingArea::new();
        progress.set_valign(gtk::Align::Start);
        progress.set_content_height(2);
        progress.set_can_target(false);
        progress.set_visible(false);
        root.add_overlay(&progress);

        let toast_label = gtk::Label::new(None);
        toast_label.add_css_class("thurm-toast");
        let toast = gtk::Revealer::new();
        toast.set_transition_type(gtk::RevealerTransitionType::Crossfade);
        toast.set_transition_duration(350);
        toast.set_child(Some(&toast_label));
        toast.set_halign(gtk::Align::Center);
        toast.set_valign(gtk::Align::End);
        toast.set_margin_bottom(12);
        toast.set_can_target(false);
        root.add_overlay(&toast);

        let offline_label = gtk::Label::new(None);
        offline_label.set_wrap(true);
        offline_label.set_lines(4);
        offline_label.set_justify(gtk::Justification::Center);
        offline_label.set_max_width_chars(48);
        offline_label.add_css_class("thurm-offline-label");
        let offline_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        offline_box.add_css_class("thurm-offline");
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 0);
        inner.add_css_class("thurm-offline-box");
        inner.set_halign(gtk::Align::Center);
        inner.set_valign(gtk::Align::Center);
        inner.set_vexpand(true);
        inner.append(&offline_label);
        offline_box.append(&inner);
        offline_box.set_visible(false);
        root.add_overlay(&offline_box);

        let lock = gtk::Image::from_icon_name("changes-prevent-symbolic");
        lock.add_css_class("thurm-lock");
        lock.set_halign(gtk::Align::End);
        lock.set_valign(gtk::Align::Start);
        lock.set_margin_top(6);
        lock.set_margin_end(8);
        lock.set_tooltip_text(Some("A password prompt is showing"));
        lock.set_visible(false);
        root.add_overlay(&lock);

        let popup_list = gtk::ListBox::new();
        popup_list.set_selection_mode(gtk::SelectionMode::Single);
        popup_list.set_can_focus(false);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_propagate_natural_width(true);
        scroll.set_child(Some(&popup_list));
        scroll.set_can_focus(false);
        let popup = gtk::Box::new(gtk::Orientation::Vertical, 0);
        popup.add_css_class("thurm-completion");
        popup.append(&scroll);
        popup.set_halign(gtk::Align::Start);
        popup.set_valign(gtk::Align::Start);
        popup.set_can_focus(false);
        popup.set_visible(false);
        root.add_overlay(&popup);

        let view = Rc::new(TermView {
            key,
            root,
            area,
            text,
            fonts: RefCell::new(fonts),
            state: RefCell::new(State::default()),
            im: gtk::IMMulticontext::new(),
            focused: StdCell::new(false),
            progress,
            progress_state: RefCell::new(None),
            progress_started: StdCell::new(None),
            toast,
            toast_label,
            toast_generation: StdCell::new(0),
            offline_box,
            offline_label,
            lock,
            popup,
            popup_list,
            tick: RefCell::new(None),
            blink_timer: RefCell::new(None),
        });
        view.im.set_client_widget(Some(&view.area));
        view.im.set_use_preedit(true);
        view.install();
        view
    }

    fn core(&self) -> Option<Rc<Core>> {
        app::with_app(|a| a.core(&self.key.host)).flatten()
    }

    fn with_core(&self, f: impl FnOnce(&Core)) {
        if self.is_offline() {
            return;
        }
        if let Some(c) = self.core() {
            f(&c);
        }
    }

    pub fn is_offline(&self) -> bool {
        self.state.borrow().offline.is_some()
    }

    pub fn snapshot_modes(&self) -> u32 {
        self.state.borrow().snapshot.info.modes
    }

    pub fn set_fonts(&self, fonts: Rc<Fonts>) {
        *self.fonts.borrow_mut() = fonts;
        self.state.borrow_mut().sent = None;
        self.send_size();
        self.area.queue_draw();
    }

    fn padding(&self) -> (f64, f64) {
        app::with_app(|a| a.ui().padding()).unwrap_or((8.0, 6.0))
    }

    /// Cells that fit the view now.
    pub fn grid_size(&self) -> (u16, u16) {
        let fonts = self.fonts.borrow();
        grid_size_for(
            self.area.width() as f64,
            self.area.height() as f64,
            &fonts,
            self.padding(),
        )
    }

    /// Cell size in device pixels (for the image protocols).
    pub fn cell_pixels(&self) -> (u16, u16) {
        let fonts = self.fonts.borrow();
        let s = self.area.scale_factor().max(1) as f64;
        ((fonts.cell_w * s) as u16, (fonts.cell_h * s) as u16)
    }

    fn send_size(&self) {
        if self.area.width() <= 0 || self.area.height() <= 0 || !self.state.borrow().subscribed {
            return;
        }
        let (cols, rows) = self.grid_size();
        let (cw, ch) = self.cell_pixels();
        if self.state.borrow().sent == Some((cols, rows, cw, ch)) {
            return;
        }
        self.state.borrow_mut().sent = Some((cols, rows, cw, ch));
        let id = self.key.id;
        if let Some(c) = self.core() {
            c.resize(id, cols, rows, cw, ch);
        }
    }

    pub fn grab_focus(&self) {
        self.area.grab_focus();
    }

    // MARK: subscription

    /// Subscribes while on screen (a mapped widget); hidden splits and tabs unsubscribe.
    fn update_subscription(&self) {
        let visible = self.area.is_mapped();
        let subscribed = self.state.borrow().subscribed;
        if visible && !subscribed {
            if let Some(c) = self.core() {
                c.subscribe(self.key.id);
                let mut st = self.state.borrow_mut();
                st.subscribed = true;
                st.sent = None;
            }
            self.send_size();
            self.area.queue_draw();
            self.maybe_show_restored_toast();
        } else if !visible && subscribed {
            if let Some(c) = self.core() {
                c.unsubscribe(self.key.id);
            }
            self.state.borrow_mut().subscribed = false;
        }
    }

    /// After a reconnect: subscribe again and re-report size and focus.
    pub fn resubscribe(&self) {
        {
            let mut st = self.state.borrow_mut();
            st.subscribed = false;
            st.sent = None;
            st.reported_focus = None;
            st.images.clear();
        }
        self.update_subscription();
        self.report_focus();
    }

    /// The pane is going away (closed, or its tab moved to a hidden workspace).
    pub fn detach(&self) {
        if self.state.borrow().subscribed
            && let Some(c) = self.core()
        {
            c.unsubscribe(self.key.id);
        }
        self.state.borrow_mut().subscribed = false;
        if let Some(t) = self.tick.borrow_mut().take() {
            t.remove();
        }
        if let Some(t) = self.blink_timer.borrow_mut().take() {
            t.remove();
        }
    }

    // MARK: focus

    fn report_focus(&self) {
        let window_active = self
            .area
            .root()
            .and_downcast::<gtk::Window>()
            .is_some_and(|w| w.is_active());
        let focused = self.focused.get() && window_active;
        if self.state.borrow().reported_focus == Some(focused) {
            return;
        }
        self.state.borrow_mut().reported_focus = Some(focused);
        if let Some(c) = self.core()
            && !self.is_offline()
        {
            c.focus(self.key.id, focused);
        }
    }

    pub fn window_activity_changed(&self) {
        self.report_focus();
        self.area.queue_draw();
    }

    pub fn set_dimmed(&self, dimmed: bool) {
        if self.state.borrow().dimmed != dimmed {
            self.state.borrow_mut().dimmed = dimmed;
            self.area.queue_draw();
        }
    }

    fn reset_blink(&self) {
        self.state.borrow_mut().blink_epoch = Some(Instant::now());
        self.area.queue_draw();
    }

    // MARK: overlays

    pub fn show_toast(&self, text: &str, secs: f64) {
        self.toast_label.set_text(text);
        self.toast.set_reveal_child(true);
        let generation = self.toast_generation.get() + 1;
        self.toast_generation.set(generation);
        let weak = self.toast.downgrade();
        let key = self.key.clone();
        glib::timeout_add_local_once(Duration::from_secs_f64(secs), move || {
            let still = app::with_app(|a| {
                a.view(&key)
                    .is_some_and(|v| v.toast_generation.get() == generation)
            })
            .unwrap_or(false);
            if still && let Some(t) = weak.upgrade() {
                t.set_reveal_child(false);
            }
        });
    }

    /// "Session restored", once per view, for a pane that survived a daemon restart.
    pub fn maybe_show_restored_toast(&self) {
        if self.state.borrow().restored_toast_shown || !self.state.borrow().subscribed {
            return;
        }
        let restored = app::with_app(|a| a.pane_info(&self.key).is_some_and(|i| i.restored))
            .unwrap_or(false);
        if restored {
            self.state.borrow_mut().restored_toast_shown = true;
            self.show_toast("Session restored", 2.5);
        }
    }

    pub fn set_progress(self: &Rc<Self>, progress: Option<thurm_proto::Progress>) {
        if *self.progress_state.borrow() == progress {
            return;
        }
        let indeterminate = progress
            .as_ref()
            .is_some_and(|p| p.state == ProgressState::Indeterminate);
        *self.progress_state.borrow_mut() = progress;
        self.progress.set_visible(self.progress_state.borrow().is_some());
        self.progress_started
            .set(indeterminate.then(Instant::now));
        self.progress.queue_draw();
        self.ensure_tick();
    }

    pub fn set_offline(&self, message: Option<String>) {
        let was = self.state.borrow().offline.is_some();
        self.offline_box.set_visible(message.is_some());
        if let Some(m) = &message {
            self.offline_label.set_text(m);
        }
        self.state.borrow_mut().offline = message;
        if was && !self.is_offline() {
            self.resubscribe();
        }
    }

    pub fn set_lock(&self, on: bool) {
        self.lock.set_visible(on);
    }

    pub fn flash(self: &Rc<Self>) {
        self.state.borrow_mut().flash_start = Some(Instant::now());
        self.ensure_tick();
    }

    // MARK: install

    fn install(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.area.set_draw_func(move |_, cr, w, h| {
            if let Some(v) = weak.upgrade() {
                v.draw(cr, w, h);
            }
        });

        let weak = Rc::downgrade(self);
        self.progress.set_draw_func(move |_, cr, w, h| {
            if let Some(v) = weak.upgrade() {
                v.draw_progress(cr, w, h);
            }
        });

        let weak = Rc::downgrade(self);
        self.area.connect_resize(move |_, _, _| {
            if let Some(v) = weak.upgrade() {
                v.send_size();
            }
        });
        let weak = Rc::downgrade(self);
        self.area.connect_map(move |_| {
            if let Some(v) = weak.upgrade() {
                v.update_subscription();
            }
        });
        let weak = Rc::downgrade(self);
        self.area.connect_unmap(move |_| {
            if let Some(v) = weak.upgrade() {
                v.update_subscription();
            }
        });

        // Input method: commits during a key press are that key's text; others (a finished
        // composition, a candidate picked with the mouse) go to the PTY as typed text.
        let weak = Rc::downgrade(self);
        self.im.connect_commit(move |_, text| {
            let Some(v) = weak.upgrade() else { return };
            let mut st = v.state.borrow_mut();
            if let Some(acc) = st.committed.as_mut() {
                acc.push_str(text);
            } else {
                drop(st);
                v.with_core(|c| c.input(v.key.id, text));
            }
        });
        let weak = Rc::downgrade(self);
        self.im.connect_preedit_start(move |_| {
            if let Some(v) = weak.upgrade() {
                v.state.borrow_mut().composing = true;
            }
        });
        let weak = Rc::downgrade(self);
        self.im.connect_preedit_changed(move |im| {
            if let Some(v) = weak.upgrade() {
                let (text, _, _) = im.preedit_string();
                v.state.borrow_mut().preedit = text.to_string();
                v.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(self);
        self.im.connect_preedit_end(move |_| {
            if let Some(v) = weak.upgrade() {
                let mut st = v.state.borrow_mut();
                st.composing = false;
                st.preedit.clear();
                drop(st);
                v.area.queue_draw();
            }
        });

        let keyc = gtk::EventControllerKey::new();
        keyc.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keyc.connect_key_pressed(move |ctl, keyval, keycode, state| {
            let Some(v) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            v.key_pressed(ctl, keyval, keycode, state)
        });
        let weak = Rc::downgrade(self);
        keyc.connect_key_released(move |ctl, keyval, keycode, state| {
            if let Some(v) = weak.upgrade() {
                v.key_released(ctl, keyval, keycode, state);
            }
        });
        let weak = Rc::downgrade(self);
        keyc.connect_modifiers(move |_, state| {
            if let Some(v) = weak.upgrade() {
                v.modifiers_changed(state);
            }
            glib::Propagation::Proceed
        });
        self.area.add_controller(keyc);

        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_enter(move |_| {
            let Some(v) = weak.upgrade() else { return };
            v.focused.set(true);
            v.im.focus_in();
            v.reset_blink();
            v.report_focus();
            let key = v.key.clone();
            app::with_app(|a| a.pane_focused(&key));
        });
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            let Some(v) = weak.upgrade() else { return };
            v.focused.set(false);
            v.im.focus_out();
            v.close_completion();
            v.report_focus();
            v.area.queue_draw();
        });
        self.area.add_controller(focus);

        let click = gtk::GestureClick::new();
        click.set_button(0);
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |g, n, x, y| {
            if let Some(v) = weak.upgrade() {
                v.button(g, MOUSE_PRESS, n, x, y);
            }
        });
        let weak = Rc::downgrade(self);
        click.connect_released(move |g, n, x, y| {
            if let Some(v) = weak.upgrade() {
                v.button(g, MOUSE_RELEASE, n, x, y);
            }
        });
        self.area.add_controller(click);

        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |ctl, x, y| {
            if let Some(v) = weak.upgrade() {
                v.motion(ctl.current_event_state(), x, y);
            }
        });
        let weak = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(v) = weak.upgrade() {
                v.state.borrow_mut().pointer = None;
                v.update_hover();
            }
        });
        self.area.add_controller(motion);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll_begin(move |_| {
            if let Some(v) = weak.upgrade() {
                v.state.borrow_mut().scroll_acc = 0.0;
            }
        });
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |ctl, _dx, dy| {
            if let Some(v) = weak.upgrade() {
                v.scroll(ctl, dy);
            }
            glib::Propagation::Stop
        });
        self.area.add_controller(scroll);

        // Drop files, URLs, text and images.
        let drop = gtk::DropTarget::new(glib::Type::INVALID, gdk::DragAction::COPY);
        drop.set_types(&[
            gdk::FileList::static_type(),
            gdk::Texture::static_type(),
            glib::GString::static_type(),
        ]);
        let weak = Rc::downgrade(self);
        drop.connect_drop(move |_, value, _, _| {
            let Some(v) = weak.upgrade() else { return false };
            v.dropped(value)
        });
        self.area.add_controller(drop);

        // Completion rows: click accepts.
        let weak = Rc::downgrade(self);
        self.popup_list.connect_row_activated(move |_, row| {
            if let Some(v) = weak.upgrade() {
                let i = row.index().max(0) as usize;
                if let Some(c) = v.state.borrow_mut().completion.as_mut() {
                    c.selected = i;
                }
                v.accept_completion();
            }
        });
    }

    fn ensure_tick(self: &Rc<Self>) {
        if self.tick.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = self.area.add_tick_callback(move |_, _| {
            let Some(v) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if v.on_tick() {
                glib::ControlFlow::Continue
            } else {
                v.tick.borrow_mut().take();
                glib::ControlFlow::Break
            }
        });
        *self.tick.borrow_mut() = Some(id);
    }

    /// One frame of animation: smooth scroll, bell flash, progress. Returns whether to keep
    /// ticking.
    fn on_tick(&self) -> bool {
        let mut more = false;
        if self.state.borrow().smooth_active {
            self.advance_smooth_scroll();
            more |= self.state.borrow().smooth_active;
            self.area.queue_draw();
        }
        if let Some(start) = self.state.borrow().flash_start {
            if start.elapsed().as_secs_f64() < FLASH {
                more = true;
            }
            self.area.queue_draw();
        }
        if self.progress_started.get().is_some() {
            self.progress.queue_draw();
            more = true;
        }
        more
    }

    // MARK: drawing

    pub fn frame_arrived(&self) {
        self.area.queue_draw();
    }

    /// Copies the pane's screen out of the library and loads new image pixels.
    fn take_snapshot(&self, core: &Core) {
        let id = self.key.id;
        let snap = core.with_grid(id, |g| {
            let info = g.info;
            let mut s = Snapshot {
                info,
                cells: g.cells.to_vec(),
                valid: true,
                ..Default::default()
            };
            for (r, c, text) in g.clusters() {
                s.clusters.insert((r, c), text);
            }
            for cell in g.cells {
                if cell.link != 0 && !s.links.contains_key(&cell.link)
                    && let Some(uri) = g.link(cell.link)
                {
                    s.links.insert(cell.link, uri);
                }
            }
            s.images = g.images();
            if info.has_peek
                && let Some((cells, clusters)) = g.peek()
            {
                s.peek = Some((cells, clusters.into_iter().collect()));
            }
            s
        });
        let Some(snap) = snap else { return };
        let mut st = self.state.borrow_mut();
        // Images: reload when the pixels behind an id changed; forget unused ones.
        for pl in &snap.images {
            let serial = core.image_serial(id, pl.image);
            if serial == 0 {
                continue;
            }
            if st.images.get(&pl.image).is_some_and(|e| e.serial == serial) {
                continue;
            }
            if let Some((w, h, data)) = core.image_argb(id, pl.image)
                && w > 0
                && h > 0
                && w <= 16384
                && h <= 16384
                && let Some(surface) = render::image_surface(w, h, data)
            {
                st.images.insert(
                    pl.image,
                    render::ImageEntry {
                        serial,
                        surface,
                        last_used: Instant::now(),
                    },
                );
            }
        }
        // Pixels no placement used for 2 s are dropped.
        st.images.retain(|id, e| {
            snap.images.iter().any(|pl| pl.image == *id) || e.last_used.elapsed() < Duration::from_secs(2)
        });
        st.snapshot = snap;
    }

    fn draw(&self, cr: &gtk::cairo::Context, w: i32, h: i32) {
        let generation_before = self.state.borrow().snapshot.info.generation;
        if let Some(core) = self.core() {
            self.take_snapshot(&core);
        }
        let Some((opacity, cursor_blink, thickness, dim_amount, theme_fg, theme_bg)) =
            app::with_app(|a| {
                let ui = a.ui();
                (
                    ui.opacity(),
                    ui.cfg.cursor.blink,
                    ui.cursor_thickness(),
                    ui.unfocused_dim(),
                    ui.theme.foreground,
                    ui.theme.background,
                )
            })
        else {
            return;
        };
        let (pad_x, pad_y) = self.padding();
        let fonts = self.fonts.borrow().clone();
        let focused = self.focused.get()
            && self
                .area
                .root()
                .and_downcast::<gtk::Window>()
                .is_some_and(|w| w.is_active());
        let mut st = self.state.borrow_mut();
        let info = st.snapshot.info;

        // Cursor blink.
        let blinking = (cursor_blink || info.cursor_blinking) && focused;
        let cursor_on = if blinking {
            let epoch = *st.blink_epoch.get_or_insert_with(Instant::now);
            let elapsed = epoch.elapsed().as_secs_f64();
            let next = ((elapsed / BLINK).floor() + 1.0) * BLINK - elapsed + 0.001;
            self.schedule_blink(next);
            ((elapsed / BLINK).floor() as u64).is_multiple_of(2)
        } else {
            true
        };

        // Smooth scrolling offset.
        let scroll_y = if st.smooth_active {
            let mut frac = st.smooth_pos - info.display_offset as f64;
            if !info.has_peek {
                frac = frac.min(0.0);
            }
            (frac.clamp(0.0, 0.999) * fonts.cell_h).round()
        } else {
            0.0
        };
        let flash = st
            .flash_start
            .map(|s| 1.0 - s.elapsed().as_secs_f64() / FLASH)
            .filter(|f| *f > 0.0)
            .unwrap_or(0.0);
        if flash == 0.0 {
            st.flash_start = None;
        }
        let preedit = st.preedit.clone();
        let params = PaintParams {
            width: w as f64,
            height: h as f64,
            pad_x,
            pad_y,
            opacity,
            focused,
            cursor_on,
            cursor_thickness: thickness,
            dim: if st.dimmed { dim_amount } else { 0.0 },
            flash,
            scroll_y,
            hover: st.hover,
            preedit: (!preedit.is_empty()).then_some(preedit.as_str()),
            theme_bg,
            theme_fg,
        };
        let State {
            snapshot, images, ..
        } = &mut *st;
        render::paint(cr, &fonts, snapshot, &params, images);

        // The input method's candidate window follows the cursor.
        let rect = gdk::Rectangle::new(
            (pad_x + info.cursor_col as f64 * fonts.cell_w) as i32,
            (pad_y + info.cursor_row as f64 * fonts.cell_h) as i32,
            fonts.cell_w as i32,
            fonts.cell_h as i32,
        );
        drop(st);
        self.im.set_cursor_location(&rect);
        if info.generation != generation_before {
            self.accessibility_update();
        }
    }

    fn schedule_blink(&self, secs: f64) {
        if self.blink_timer.borrow().is_some() {
            return;
        }
        let area = self.area.downgrade();
        let key = self.key.clone();
        let id = glib::timeout_add_local_once(Duration::from_secs_f64(secs.max(0.01)), move || {
            app::with_app(|a| {
                if let Some(v) = a.view(&key) {
                    v.blink_timer.borrow_mut().take();
                }
            });
            if let Some(a) = area.upgrade() {
                a.queue_draw();
            }
        });
        *self.blink_timer.borrow_mut() = Some(id);
    }

    fn draw_progress(&self, cr: &gtk::cairo::Context, w: i32, h: i32) {
        let Some(p) = *self.progress_state.borrow() else { return };
        let (r, g, b) = match p.state {
            ProgressState::Error => (1.0, 0.27, 0.23),
            ProgressState::Paused => (1.0, 0.8, 0.0),
            _ => (0.21, 0.52, 0.89),
        };
        cr.set_source_rgb(r, g, b);
        let w = w as f64;
        if p.state == ProgressState::Indeterminate {
            let seg = (w * 0.25).max(40.0);
            let t = self
                .progress_started
                .get()
                .map_or(0.0, |s| s.elapsed().as_secs_f64());
            // Back and forth over 1.2 s, eased.
            let phase = (t / 1.2) % 2.0;
            let u = if phase < 1.0 { phase } else { 2.0 - phase };
            let eased = u * u * (3.0 - 2.0 * u);
            let x = -seg + (w + seg) * eased;
            cr.rectangle(x, 0.0, seg, h as f64);
        } else {
            let pct = p.percent.unwrap_or(100).min(100) as f64 / 100.0;
            cr.rectangle(0.0, 0.0, w * pct, h as f64);
        }
        let _ = cr.fill();
    }

    // MARK: keyboard

    fn key_pressed(
        self: &Rc<Self>,
        ctl: &gtk::EventControllerKey,
        keyval: gdk::Key,
        keycode: u32,
        state: gdk::ModifierType,
    ) -> glib::Propagation {
        if self.is_offline() {
            return glib::Propagation::Stop;
        }
        self.area.set_cursor_from_name(Some("none"));
        self.state.borrow_mut().smooth_active = false;
        self.state.borrow_mut().last_key = Some(Instant::now());
        self.reset_blink();

        let composing = self.state.borrow().composing;
        if !composing && self.completion_key(keyval, state) {
            return glib::Propagation::Stop;
        }
        // The app's shortcuts, except while the input method composes (its keys pick candidates).
        if !composing
            && let Some(action) = app::with_app(|a| a.action_for_key(keyval, keycode, state)).flatten()
        {
            app::with_app(|a| a.activate(action));
            return glib::Propagation::Stop;
        }
        let Some(event) = ctl.current_event() else {
            return glib::Propagation::Proceed;
        };
        // GDK has no repeat flag: a second press without a release in between is a repeat.
        let action = if self.state.borrow_mut().down.insert(keycode) {
            keys::PRESS
        } else {
            keys::REPEAT
        };

        let had_preedit = composing;
        self.state.borrow_mut().committed = Some(String::new());
        let handled = self.im.filter_keypress(&event);
        let committed = self.state.borrow_mut().committed.take().unwrap_or_default();
        let still_composing = self.state.borrow().composing;

        if !committed.is_empty() {
            if had_preedit {
                // A composition (dead key, IME) finished: plain text, not a key press.
                self.with_core(|c| c.input(self.key.id, &committed));
                if !handled {
                    self.send_key(&event, keyval, keycode, state, action, None);
                }
            } else {
                let text = keys::is_printable(&committed).then_some(committed.as_str());
                self.send_key(&event, keyval, keycode, state, action, text);
            }
            self.maybe_refresh_completion();
            return glib::Propagation::Stop;
        }
        if handled || had_preedit || still_composing {
            // The input method consumed the key (preedit update or cancel).
            return glib::Propagation::Stop;
        }
        self.send_key(&event, keyval, keycode, state, action, None);
        self.maybe_refresh_completion();
        glib::Propagation::Stop
    }

    fn key_released(
        &self,
        ctl: &gtk::EventControllerKey,
        keyval: gdk::Key,
        keycode: u32,
        state: gdk::ModifierType,
    ) {
        self.state.borrow_mut().down.remove(&keycode);
        // Released to the terminal exactly when it got the press: not for a shortcut's or the
        // input method's key, and whatever the modifiers and the input method do now.
        let delivered = self.state.borrow_mut().delivered.remove(&keycode);
        if self.is_offline() {
            return;
        }
        let Some(event) = ctl.current_event() else { return };
        // The input method sees every release (it may track the key).
        self.im.filter_keypress(&event);
        if delivered {
            self.send_key(&event, keyval, keycode, state, keys::RELEASE, None);
        }
    }

    fn modifiers_changed(&self, state: gdk::ModifierType) {
        let link_mod = state.contains(gdk::ModifierType::CONTROL_MASK);
        if self.state.borrow().link_mod != link_mod {
            self.state.borrow_mut().link_mod = link_mod;
            self.update_hover();
        }
    }

    fn send_key(
        &self,
        event: &gdk::Event,
        keyval: gdk::Key,
        keycode: u32,
        state: gdk::ModifierType,
        action: u8,
        text: Option<&str>,
    ) {
        if action != keys::RELEASE {
            self.state.borrow_mut().delivered.insert(keycode);
        }
        let mods = keys::mods(state);
        let id = self.key.id;
        if let Some((named, keypad)) = keys::named(keyval) {
            // Keypad keys keep their text (digits, operators) for the legacy encoding.
            let text = if keypad { text } else { None };
            self.with_core(|c| c.key(id, keys::KIND_NAMED, named, mods, action, text, 0, 0));
            return;
        }
        let group = event
            .downcast_ref::<gdk::KeyEvent>()
            .map_or(0, |k| k.layout() as i32);
        let (code, shifted, base) = keys::text_codes(&self.area.display(), keycode, group);
        if code == 0 {
            if let Some(t) = text
                && action != keys::RELEASE
            {
                self.with_core(|c| c.input(id, t));
            }
            return;
        }
        let text = if action == keys::RELEASE { None } else { text };
        self.with_core(|c| c.key(id, keys::KIND_TEXT, code, mods, action, text, shifted, base));
    }

    // MARK: completion

    fn completion_key(self: &Rc<Self>, keyval: gdk::Key, state: gdk::ModifierType) -> bool {
        let mods = keys::mods(state) & !keys::MOD_CAPS_LOCK;
        let visible = self.state.borrow().completion.is_some();
        if !visible {
            if (keyval == gdk::Key::Tab) && mods == 0 {
                return self.start_completion();
            }
            return false;
        }
        match keyval {
            gdk::Key::Tab | gdk::Key::Down => self.move_completion(1),
            gdk::Key::ISO_Left_Tab | gdk::Key::Up => self.move_completion(-1),
            gdk::Key::Return | gdk::Key::KP_Enter => self.accept_completion(),
            gdk::Key::Right if mods == 0 => self.accept_completion(),
            gdk::Key::Escape => self.close_completion(),
            _ => return false,
        }
        true
    }

    fn completion_allowed(&self) -> bool {
        let enabled = app::with_app(|a| a.ui().cfg.terminal.tab_completion).unwrap_or(false);
        let at_prompt = app::with_app(|a| a.pane_info(&self.key).is_some_and(|i| i.at_prompt))
            .unwrap_or(false);
        enabled
            && at_prompt
            && !self.is_offline()
            && self.snapshot_modes() & render::MODE_ALT_SCREEN == 0
    }

    /// Tab at a prompt: insert the only match or the common prefix, list the rest.
    fn start_completion(self: &Rc<Self>) -> bool {
        if !self.completion_allowed() {
            return false;
        }
        let Some(core) = self.core() else { return false };
        let resp = core.request_timeout(&json!({"Complete": {"pane": self.key.id}}), 1000);
        let Some(c) = parse_completions(&resp) else { return false };
        if c.items.is_empty() {
            return false;
        }
        if c.items.len() == 1 {
            let item = &c.items[0];
            self.insert_completion(&item.text, &c.word, !is_directory(item));
            return true;
        }
        let prefix = common_prefix(c.items.iter().map(|i| i.text.as_str()));
        let mut word = c.word.clone();
        if prefix.chars().count() > word.chars().count() {
            self.insert_completion(&prefix, &word, false);
            word = prefix;
        }
        self.show_completion(word, c.items);
        true
    }

    fn insert_completion(&self, text: &str, word: &str, last: bool) {
        let (Some(escaped), Some(typed)) = (model::shell_escape(text), model::shell_escape(word))
        else {
            return;
        };
        let mut out = match escaped.strip_prefix(&typed) {
            Some(rest) => rest.to_string(),
            None => "\u{7f}".repeat(typed.chars().count()) + &escaped,
        };
        if last {
            out.push(' ');
        }
        self.with_core(|c| c.input(self.key.id, &out));
    }

    fn show_completion(&self, word: String, items: Vec<CompletionItem>) {
        while let Some(row) = self.popup_list.first_child() {
            self.popup_list.remove(&row);
        }
        for item in &items {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.add_css_class("thurm-completion-row");
            let icon = gtk::Image::from_icon_name(match item.kind {
                thurm_proto::CompletionKind::Command => "utilities-terminal-symbolic",
                thurm_proto::CompletionKind::Subcommand => "go-next-symbolic",
                thurm_proto::CompletionKind::Flag => "emoji-flags-symbolic",
                thurm_proto::CompletionKind::Directory => "folder-symbolic",
                thurm_proto::CompletionKind::File => "text-x-generic-symbolic",
                _ => "format-justify-left-symbolic",
            });
            icon.add_css_class("dim-label");
            row.append(&icon);
            let name = gtk::Label::new(Some(&item.text));
            name.set_xalign(0.0);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            name.set_hexpand(true);
            row.append(&name);
            if let Some(d) = &item.description {
                let desc = gtk::Label::new(Some(d));
                desc.set_xalign(1.0);
                desc.set_ellipsize(gtk::pango::EllipsizeMode::End);
                desc.set_max_width_chars(32);
                desc.add_css_class("dim-label");
                desc.add_css_class("thurm-completion-detail");
                row.append(&desc);
            }
            self.popup_list.append(&row);
        }
        let fonts = self.fonts.borrow().clone();
        let (pad_x, pad_y) = self.padding();
        let info = self.state.borrow().snapshot.info;
        let x = pad_x + info.cursor_col as f64 * fonts.cell_w - 8.0;
        let below = pad_y + (info.cursor_row as f64 + 1.0) * fonts.cell_h + 2.0;
        let row_h = fonts.cell_h + 8.0;
        let height = items.len().min(10) as f64 * row_h + 8.0;
        let top = if below + height > self.area.height() as f64 {
            (pad_y + info.cursor_row as f64 * fonts.cell_h - height - 2.0).max(0.0)
        } else {
            below
        };
        let width = 560.0f64.min(self.area.width() as f64 - 8.0);
        self.popup.set_margin_start(x.max(0.0).min((self.area.width() as f64 - 220.0).max(0.0)) as i32);
        self.popup.set_margin_top(top as i32);
        if let Some(sw) = self.popup.first_child().and_downcast::<gtk::ScrolledWindow>() {
            sw.set_max_content_height(height as i32);
            sw.set_max_content_width(width as i32);
        }
        self.popup.set_visible(true);
        if let Some(row) = self.popup_list.row_at_index(0) {
            self.popup_list.select_row(Some(&row));
        }
        self.state.borrow_mut().completion = Some(Completion {
            word,
            items,
            selected: 0,
        });
    }

    fn move_completion(&self, delta: i32) {
        let mut st = self.state.borrow_mut();
        let Some(c) = st.completion.as_mut() else { return };
        let n = c.items.len() as i32;
        c.selected = ((c.selected as i32 + delta).rem_euclid(n)) as usize;
        let sel = c.selected;
        drop(st);
        if let Some(row) = self.popup_list.row_at_index(sel as i32) {
            self.popup_list.select_row(Some(&row));
            row.grab_focus();
            self.area.grab_focus();
        }
    }

    fn accept_completion(&self) {
        let Some(c) = self.state.borrow_mut().completion.take() else { return };
        self.popup.set_visible(false);
        if let Some(item) = c.items.get(c.selected) {
            self.insert_completion(&item.text, &c.word, !is_directory(item));
        }
    }

    pub fn close_completion(&self) {
        self.state.borrow_mut().completion = None;
        self.popup.set_visible(false);
    }

    /// Typing while the list shows refreshes it (debounced; only the latest answer counts).
    fn maybe_refresh_completion(self: &Rc<Self>) {
        if self.state.borrow().completion.is_none() {
            return;
        }
        let query = {
            let mut st = self.state.borrow_mut();
            st.completion_query += 1;
            st.completion_query
        };
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(60), move || {
            let Some(v) = weak.upgrade() else { return };
            if v.state.borrow().completion_query != query {
                return;
            }
            let Some(core) = v.core() else { return };
            let weak = Rc::downgrade(&v);
            core.request_async(&json!({"Complete": {"pane": v.key.id}}), 2000, move |resp| {
                let Some(v) = weak.upgrade() else { return };
                if v.state.borrow().completion_query != query || v.state.borrow().completion.is_none() {
                    return;
                }
                match parse_completions(&resp).filter(|c| !c.items.is_empty()) {
                    Some(c) if v.completion_allowed() => v.show_completion(c.word, c.items),
                    _ => v.close_completion(),
                }
            });
        });
    }

    // MARK: mouse

    fn cell_at(&self, x: f64, y: f64) -> (u16, u16, bool) {
        let fonts = self.fonts.borrow();
        let (pad_x, pad_y) = self.padding();
        let st = self.state.borrow();
        let (cols, rows) = if st.snapshot.valid {
            (st.snapshot.info.cols, st.snapshot.info.rows)
        } else {
            drop(st);
            return (0, 0, false);
        };
        let scroll_y = if st.smooth_active {
            ((st.smooth_pos - st.snapshot.info.display_offset as f64).clamp(0.0, 0.999) * fonts.cell_h).round()
        } else {
            0.0
        };
        let fx = ((x - pad_x) / fonts.cell_w).max(0.0);
        let fy = ((y - pad_y - scroll_y) / fonts.cell_h).max(0.0);
        let col = (fx.floor() as u32).min(cols.saturating_sub(1) as u32) as u16;
        let row = (fy.floor() as u32).min(rows.saturating_sub(1) as u32) as u16;
        (col, row, fx.fract() >= 0.5)
    }

    fn send_mouse(&self, kind: u8, button: u8, mods: u8, x: f64, y: f64) {
        let (col, row, right_half) = self.cell_at(x, y);
        let clicks = self.state.borrow().clicks.max(1);
        let s = self.area.scale_factor().max(1) as f64;
        let (pad_x, pad_y) = self.padding();
        let id = self.key.id;
        self.with_core(|c| {
            c.mouse(
                id,
                kind,
                button,
                mods,
                clicks,
                col,
                row,
                right_half,
                ((x - pad_x).max(0.0) * s) as u32,
                ((y - pad_y).max(0.0) * s) as u32,
            )
        });
    }

    fn button(self: &Rc<Self>, g: &gtk::GestureClick, kind: u8, n: i32, x: f64, y: f64) {
        if self.is_offline() {
            return;
        }
        let state = g.current_event_state();
        let mods = keys::mods(state);
        let button = match g.current_button() {
            1 => BUTTON_LEFT,
            2 => BUTTON_MIDDLE,
            3 => BUTTON_RIGHT,
            8 => BUTTON_BACK,
            9 => BUTTON_FORWARD,
            _ => BUTTON_MIDDLE,
        };
        let reporting = self.snapshot_modes() & render::MODE_MOUSE_ANY != 0;
        if kind == MOUSE_PRESS {
            self.grab_focus();
            self.close_completion();
            if button == BUTTON_LEFT && state.contains(gdk::ModifierType::CONTROL_MASK) {
                let (col, row, _) = self.cell_at(x, y);
                let link = self.state.borrow().snapshot.link_at(row as usize, col as usize);
                if let Some((url, _)) = link {
                    let key = self.key.clone();
                    app::with_app(|a| a.open_link(&url, &key));
                    return;
                }
            }
            // Middle click pastes the primary selection (the Linux convention).
            if button == BUTTON_MIDDLE && (!reporting || mods & keys::MOD_SHIFT != 0) {
                let key = self.key.clone();
                app::with_app(|a| a.paste_primary(&key));
                return;
            }
            if button == BUTTON_RIGHT && (!reporting || mods & keys::MOD_SHIFT != 0) {
                self.context_menu(x, y);
                return;
            }
            let mut st = self.state.borrow_mut();
            st.pressed = Some(button);
            st.clicks = n.clamp(1, 3) as u8;
        } else {
            let mut st = self.state.borrow_mut();
            if st.pressed != Some(button) {
                return;
            }
            st.pressed = None;
        }
        self.send_mouse(kind, button, mods, x, y);
        if kind == MOUSE_RELEASE && button == BUTTON_LEFT && !reporting {
            let key = self.key.clone();
            app::with_app(|a| a.selection_finished(&key));
        }
    }

    fn context_menu(&self, x: f64, y: f64) {
        let menu = gio::Menu::new();
        let s1 = gio::Menu::new();
        s1.append(Some("Copy"), Some("app.copy"));
        s1.append(Some("Paste"), Some("app.paste"));
        menu.append_section(None, &s1);
        let s2 = gio::Menu::new();
        s2.append(Some("Split Right"), Some("app.split_right"));
        s2.append(Some("Split Down"), Some("app.split_down"));
        menu.append_section(None, &s2);
        let s3 = gio::Menu::new();
        s3.append(Some("Clear"), Some("app.clear_screen"));
        menu.append_section(None, &s3);
        let pop = gtk::PopoverMenu::from_model(Some(&menu));
        pop.set_parent(&self.area);
        pop.set_has_arrow(false);
        pop.set_halign(gtk::Align::Start);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
    }

    fn motion(&self, state: gdk::ModifierType, x: f64, y: f64) {
        if self.state.borrow().pointer.is_none() || self.area.cursor().is_some_and(|c| c.name().as_deref() == Some("none")) {
            self.area.set_cursor_from_name(Some("text"));
        }
        self.state.borrow_mut().pointer = Some((x, y));
        self.state.borrow_mut().link_mod = state.contains(gdk::ModifierType::CONTROL_MASK);
        self.update_hover();
        if self.is_offline() {
            return;
        }
        let mods = keys::mods(state);
        let pressed = self.state.borrow().pressed;
        if let Some(button) = pressed {
            self.send_mouse(MOUSE_MOVE, button, mods, x, y);
        } else if self.snapshot_modes() & render::MODE_MOUSE_MOTION != 0 {
            self.send_mouse(MOUSE_MOVE, BUTTON_NONE, mods, x, y);
        }
    }

    /// With Ctrl held, underline the link under the pointer.
    fn update_hover(&self) {
        let (pointer, link_mod) = {
            let st = self.state.borrow();
            (st.pointer, st.link_mod)
        };
        let hover = match pointer.filter(|_| link_mod) {
            Some((x, y)) => {
                let (col, row, _) = self.cell_at(x, y);
                self.state
                    .borrow()
                    .snapshot
                    .link_at(row as usize, col as usize)
                    .map_or(Hover::None, |(_, h)| h)
            }
            None => Hover::None,
        };
        if self.state.borrow().hover != hover {
            self.state.borrow_mut().hover = hover;
            self.area.set_cursor_from_name(Some(if hover == Hover::None { "text" } else { "pointer" }));
            self.area.queue_draw();
        }
    }

    fn scroll(self: &Rc<Self>, ctl: &gtk::EventControllerScroll, dy: f64) {
        if self.is_offline() {
            return;
        }
        let (multiplier, smooth_cfg) =
            app::with_app(|a| (a.ui().scroll_multiplier(), a.ui().cfg.terminal.smooth_scroll))
                .unwrap_or((3.0, true));
        let cell_h = self.fonts.borrow().cell_h;
        let modes = self.snapshot_modes();
        let raw = modes & render::MODE_MOUSE_ANY != 0;
        let alt = modes & render::MODE_ALT_SCREEN != 0;
        let precise = ctl.unit() == gdk::ScrollUnit::Surface;
        let mut delta = if precise { dy / cell_h } else { dy };
        if !precise && delta != 0.0 {
            delta = delta.signum() * delta.abs().round().max(1.0);
        }
        // Positive lines are towards older output.
        let delta = -delta;
        let valid = self.state.borrow().snapshot.valid;
        if !raw && !alt && smooth_cfg && precise && valid {
            let info = self.state.borrow().snapshot.info;
            {
                let mut st = self.state.borrow_mut();
                if !st.smooth_active {
                    st.smooth_active = true;
                    st.smooth_pos = info.display_offset as f64;
                    st.sent_offset = info.display_offset;
                }
                st.smooth_pos =
                    (st.smooth_pos + delta * multiplier).clamp(0.0, info.history_size as f64);
                st.last_smooth_input = Some(Instant::now());
            }
            self.ensure_tick();
            self.area.queue_draw();
            return;
        }
        let whole = {
            let mut st = self.state.borrow_mut();
            st.scroll_acc += delta * if raw { 1.0 } else { multiplier };
            let whole = st.scroll_acc.trunc();
            st.scroll_acc -= whole;
            whole as i32
        };
        if whole == 0 {
            return;
        }
        let pointer = ctl
            .current_event()
            .and_then(|e| e.position())
            .unwrap_or((0.0, 0.0));
        let (col, row, _) = self.cell_at(pointer.0, pointer.1);
        let mods = keys::mods(ctl.current_event_state()) & !keys::MOD_CAPS_LOCK;
        let id = self.key.id;
        self.with_core(|c| c.wheel(id, whole, col, row, mods));
    }

    fn advance_smooth_scroll(&self) {
        let mut st = self.state.borrow_mut();
        let idle = st
            .last_smooth_input
            .map_or(1.0, |t| t.elapsed().as_secs_f64());
        if idle > 0.08 {
            let target = st.smooth_pos.round();
            st.smooth_pos += (target - st.smooth_pos) * 0.35;
            if (target - st.smooth_pos).abs() < 0.01 {
                st.smooth_pos = target;
            }
        }
        let want = (st.smooth_pos + 0.0001).floor().max(0.0) as u32;
        if want != st.sent_offset {
            st.sent_offset = want;
            let id = self.key.id;
            drop(st);
            self.with_core(|c| c.send(&json!({"Scroll": {"pane": id, "scroll": {"Offset": want}}})));
            st = self.state.borrow_mut();
        }
        let offset = st.snapshot.info.display_offset;
        let settled = st.smooth_pos.fract() == 0.0 && offset == st.sent_offset;
        let overridden = idle > 0.3 && offset != st.sent_offset;
        if idle > 0.08 && (settled || overridden) {
            st.smooth_active = false;
        }
    }

    // MARK: drop and paste

    fn dropped(self: &Rc<Self>, value: &glib::Value) -> bool {
        if self.is_offline() {
            return false;
        }
        self.grab_focus();
        if let Ok(files) = value.get::<gdk::FileList>() {
            let text: Vec<String> = files
                .files()
                .iter()
                .filter_map(|f| f.path())
                .filter_map(|p| model::shell_escape(&p.to_string_lossy()))
                .collect();
            if text.is_empty() {
                return false;
            }
            let key = self.key.clone();
            let text = text.join(" ") + " ";
            app::with_app(|a| a.paste_text(&key, text));
            return true;
        }
        if let Ok(texture) = value.get::<gdk::Texture>() {
            let key = self.key.clone();
            app::with_app(|a| a.paste_image(&key, &texture));
            return true;
        }
        if let Ok(text) = value.get::<String>() {
            let key = self.key.clone();
            app::with_app(|a| a.paste_text(&key, text));
            return true;
        }
        false
    }

    // MARK: accessibility

    /// Announces new output of the focused pane to screen readers (batched, without echoing
    /// what the user just typed).
    fn accessibility_update(&self) {
        let rows: Vec<String> = {
            let st = self.state.borrow();
            (0..st.snapshot.rows())
                .map(|r| st.snapshot.row_text(r).0.trim_end().to_string())
                .collect()
        };
        // The screen as text, the caret at the cursor cell.
        {
            let st = self.state.borrow();
            let info = st.snapshot.info;
            let mut caret = 0usize;
            for (r, row) in rows.iter().enumerate() {
                if r == info.cursor_row as usize {
                    let (_, map) = st.snapshot.row_text(r);
                    let col = map
                        .iter()
                        .position(|c| *c >= info.cursor_col)
                        .unwrap_or(map.len())
                        .min(row.chars().count());
                    caret += col;
                    break;
                }
                caret += row.chars().count() + 1;
            }
            drop(st);
            self.text.set_text(rows.join("\n"), caret as u32);
        }
        let old = std::mem::replace(&mut self.state.borrow_mut().last_rows, rows.clone());
        if !self.focused.get() || old.is_empty() {
            return;
        }
        let new = new_rows(&old, &rows);
        if new.is_empty() {
            return;
        }
        let typed_recently = self
            .state
            .borrow()
            .last_key
            .is_some_and(|t| t.elapsed() < Duration::from_millis(500));
        let cursor_row = self.state.borrow().snapshot.info.cursor_row as usize;
        if typed_recently && new.len() <= 1 && new.first() == rows.get(cursor_row) {
            return;
        }
        let mut st = self.state.borrow_mut();
        st.announce_queue.extend(new);
        while st.announce_queue.len() > 20 {
            st.announce_queue.pop_front();
        }
        if st.announce_scheduled {
            return;
        }
        st.announce_scheduled = true;
        drop(st);
        let key = self.key.clone();
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            app::with_app(|a| {
                if let Some(v) = a.view(&key) {
                    let mut st = v.state.borrow_mut();
                    st.announce_scheduled = false;
                    let text: String = st.announce_queue.drain(..).collect::<Vec<_>>().join("\n");
                    drop(st);
                    let text: String = text.chars().take(1000).collect();
                    v.area.announce(&text, gtk::AccessibleAnnouncementPriority::Medium);
                }
            });
        });
    }
}

/// Rows of `new` not in `old`, after aligning the two by the scroll shift that matches most.
fn new_rows(old: &[String], new: &[String]) -> Vec<String> {
    let n = new.len() as i32;
    let mut best = (0, -1i32);
    for i in 0..n.min(12) {
        for shift in [i, -i] {
            let matches = (0..n)
                .filter(|&r| {
                    let o = r + shift;
                    o >= 0 && (o as usize) < old.len() && old[o as usize] == new[r as usize]
                })
                .count() as i32;
            if matches > best.1 {
                best = (shift, matches);
            }
            if i == 0 {
                break;
            }
        }
    }
    let shift = best.0;
    (0..n)
        .filter(|&r| {
            let o = r + shift;
            !(o >= 0 && (o as usize) < old.len() && old[o as usize] == new[r as usize])
        })
        .map(|r| new[r as usize].clone())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn grid_size_for(w: f64, h: f64, fonts: &Fonts, pad: (f64, f64)) -> (u16, u16) {
    let cols = ((w - 2.0 * pad.0) / fonts.cell_w).floor().clamp(2.0, 1000.0) as u16;
    let rows = ((h - 2.0 * pad.1) / fonts.cell_h).floor().clamp(1.0, 1000.0) as u16;
    (cols, rows)
}

fn parse_completions(resp: &Value) -> Option<Completions> {
    serde_json::from_value(resp.get("Completions")?.clone()).ok()
}

fn is_directory(item: &CompletionItem) -> bool {
    item.kind == thurm_proto::CompletionKind::Directory || item.text.ends_with('/')
}

fn common_prefix<'a>(mut items: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = items.next() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for s in items {
        let n = prefix.iter().zip(s.chars()).take_while(|(a, b)| **a == *b).count();
        prefix.truncate(n);
    }
    prefix.into_iter().collect()
}

/// Lets the app reach a view's info-driven overlays.
pub fn info_updated(view: &Rc<TermView>, info: &PaneInfo, lock: bool) {
    view.set_progress(info.progress);
    view.set_lock(lock);
    view.maybe_show_restored_toast();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix() {
        assert_eq!(common_prefix(["checkout", "cherry-pick", "check"].into_iter()), "che");
        assert_eq!(common_prefix(["a"].into_iter()), "a");
    }

    #[test]
    fn rows_after_scroll() {
        let old: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let new: Vec<String> = ["b", "c", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(new_rows(&old, &new), vec!["d"]);
        assert!(new_rows(&old, &old).is_empty());
    }
}
