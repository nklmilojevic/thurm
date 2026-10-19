//! The page's view of [`Session`]: pane ids as numbers, requests and events as JSON, cells
//! as a `Uint32Array`.

use thurm_proto::{KeyAction, PaneId, PaneSize, Request};
use wasm_bindgen::prelude::*;

use crate::session::{KeyResult, Session};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = crypto, js_name = getRandomValues)]
    fn get_random_values(buf: &mut [u8]);
}

/// libghostty-vt has no source of secure random bytes on wasm32-freestanding: it gets the
/// browser's.
unsafe extern "C" fn random_secure(_: *mut std::ffi::c_void, buf: *mut u8, len: usize) -> bool {
    if buf.is_null() {
        return len == 0;
    }
    let out = unsafe { std::slice::from_raw_parts_mut(buf, len) };
    // getRandomValues fills at most 64 KiB per call.
    for chunk in out.chunks_mut(65_536) {
        get_random_values(chunk);
    }
    true
}

#[wasm_bindgen(start)]
fn start() {
    use ghostty_vt_rs_sys as ffi;
    let f: ffi::SysRandomSecureFn = Some(random_secure);
    unsafe {
        ffi::ghostty_sys_set(
            ffi::SysOption::GHOSTTY_SYS_OPT_RANDOM_SECURE,
            f.map_or(std::ptr::null(), |f| f as *const std::ffi::c_void),
        );
    }
}

fn pane(id: f64) -> PaneId {
    id as PaneId
}

fn size(cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> PaneSize {
    PaneSize {
        cols,
        rows,
        cell_width,
        cell_height,
    }
}

#[wasm_bindgen]
pub struct Client {
    session: Session,
}

/// One pane's repaint: `meta` (JSON: size, cursor, colors, dirty rows, clusters, links,
/// title) and the dirty rows' cells.
#[wasm_bindgen]
pub struct DrawOut {
    meta: String,
    cells: Vec<u32>,
}

#[wasm_bindgen]
impl DrawOut {
    #[wasm_bindgen(getter)]
    pub fn meta(&self) -> String {
        self.meta.clone()
    }

    pub fn take_cells(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.cells)
    }
}

#[wasm_bindgen]
impl Client {
    #[wasm_bindgen(constructor)]
    pub fn new(dark: bool) -> Client {
        Client {
            session: Session::new(dark),
        }
    }

    pub fn receive(&mut self, bytes: &[u8]) -> Result<(), JsError> {
        self.session.receive(bytes).map_err(|e| JsError::new(&e))
    }

    pub fn take_outgoing(&mut self) -> Vec<u8> {
        self.session.take_outgoing()
    }

    /// A JSON array of events (see [`Session::take_events`]).
    pub fn take_events(&mut self) -> String {
        serde_json::to_string(&self.session.take_events()).unwrap_or_else(|_| "[]".into())
    }

    /// A `Request` as serde JSON (`"ListPanes"`, `{"AnswerPermission": {…}}`); returns its id.
    pub fn request(&mut self, json: &str) -> Result<f64, JsError> {
        let request: Request =
            serde_json::from_str(json).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(self.session.request(request) as f64)
    }

    pub fn subscribe(&mut self, id: f64, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        self.session
            .subscribe(pane(id), size(cols, rows, cell_width, cell_height));
    }

    pub fn unsubscribe(&mut self, id: f64) {
        self.session.unsubscribe(pane(id));
    }

    pub fn resize(&mut self, id: f64, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        self.session
            .resize(pane(id), size(cols, rows, cell_width, cell_height));
    }

    pub fn focus(&mut self, id: f64, focused: bool) {
        self.session.focus(pane(id), focused);
    }

    /// `action`: 0 press, 1 repeat, 2 release. True when the key went to the terminal (the
    /// page prevents the browser's default).
    pub fn key(&mut self, id: f64, code: &str, key: &str, mods: u8, action: u8) -> bool {
        let action = match action {
            1 => KeyAction::Repeat,
            2 => KeyAction::Release,
            _ => KeyAction::Press,
        };
        matches!(
            self.session.key(pane(id), code, key, mods, action),
            KeyResult::Sent
        )
    }

    pub fn text(&mut self, id: f64, text: &str) {
        self.session.text(pane(id), text);
    }

    pub fn paste(&mut self, id: f64, text: &str) {
        self.session.paste(pane(id), text);
    }

    /// `kind`: 0 press, 1 release, 2 move. `button`: 0 left, 1 middle, 2 right, 3 back,
    /// 4 forward, other none.
    #[allow(clippy::too_many_arguments)]
    pub fn mouse(
        &mut self,
        id: f64,
        kind: u8,
        button: u8,
        mods: u8,
        clicks: u8,
        col: u16,
        row: u16,
        right_half: bool,
        x: u32,
        y: u32,
    ) {
        self.session.mouse(
            pane(id),
            kind,
            button,
            mods,
            clicks,
            col,
            row,
            right_half,
            x,
            y,
        );
    }

    pub fn wheel(&mut self, id: f64, lines: i32, col: u16, row: u16, mods: u8) {
        self.session.wheel(pane(id), lines, col, row, mods);
    }

    pub fn selection_text(&self, id: f64) -> String {
        self.session.selection_text(pane(id))
    }

    pub fn draw(&mut self, id: f64) -> Option<DrawOut> {
        let d = self.session.draw(pane(id))?;
        Some(DrawOut {
            meta: serde_json::to_string(&d).unwrap_or_default(),
            cells: d.cells,
        })
    }

    pub fn invalidate(&mut self, id: f64) {
        self.session.invalidate(pane(id));
    }

    pub fn next_deadline_ms(&self) -> f64 {
        self.session.next_deadline_ms()
    }

    pub fn tick(&mut self) {
        self.session.tick();
    }
}
