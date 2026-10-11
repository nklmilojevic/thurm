//! What a pane runs, as its icon shows it (PaneIcon.swift): pane headers draw a monochrome
//! glyph, sidebar rows a small app-style tile.

use gtk::cairo;
use thurm_proto::PaneInfo;

use crate::model;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneIcon {
    Shell,
    /// A program other than the shell in the foreground (htop, vim, a dev server).
    Program,
    Claude,
    Codex,
    /// Any other coding agent.
    Agent,
}

impl PaneIcon {
    pub fn of(info: Option<&PaneInfo>) -> PaneIcon {
        match info.and_then(|i| i.agent.as_ref()).map(|a| a.kind.to_ascii_lowercase()) {
            Some(k) if k == "claude" => PaneIcon::Claude,
            Some(k) if k == "codex" => PaneIcon::Codex,
            Some(_) => PaneIcon::Agent,
            None if info.is_some_and(model::has_running_process) => PaneIcon::Program,
            None => PaneIcon::Shell,
        }
    }

    /// The glyph in `size`×`size` at the origin, in the current source color.
    pub fn draw_glyph(self, cr: &cairo::Context, size: f64) {
        match self {
            PaneIcon::Shell => prompt(cr, 0.0, 0.0, size, (size / 9.0).max(1.2)),
            PaneIcon::Claude | PaneIcon::Codex => logo(cr, self, 0.0, 0.0, size),
            PaneIcon::Program => gauge(cr, size),
            PaneIcon::Agent => sparkle(cr, 0.0, 0.0, size),
        }
    }

    /// The tile in `size`×`size` at the origin.
    pub fn draw_tile(self, cr: &cairo::Context, size: f64) {
        let (fill, ink): ((f64, f64, f64), (f64, f64, f64)) = match self {
            PaneIcon::Claude => ((0.85, 0.47, 0.34), (1.0, 1.0, 1.0)),
            PaneIcon::Codex => ((1.0, 1.0, 1.0), (0.0, 0.0, 0.0)),
            PaneIcon::Agent => ((0.35, 0.38, 0.9), (1.0, 1.0, 1.0)),
            PaneIcon::Shell | PaneIcon::Program => ((0.1, 0.1, 0.1), (0.35, 0.85, 0.4)),
        };
        rounded(cr, 0.5, 0.5, size - 1.0, size - 1.0, size * 0.26);
        cr.set_source_rgb(fill.0, fill.1, fill.2);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(1.0, 1.0, 1.0, if self == PaneIcon::Codex { 0.0 } else { 0.14 });
        cr.set_line_width(1.0);
        let _ = cr.stroke();
        let inset = size * 0.22;
        let inner = size - 2.0 * inset;
        cr.set_source_rgb(ink.0, ink.1, ink.2);
        match self {
            PaneIcon::Shell | PaneIcon::Program => prompt(cr, inset, inset, inner, (size / 14.0).max(1.0)),
            PaneIcon::Claude | PaneIcon::Codex => logo(cr, self, inset, inset, inner),
            PaneIcon::Agent => sparkle(cr, inset, inset, inner),
        }
    }
}

pub fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::FRAC_PI_2;
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, 2.0 * FRAC_PI_2);
    cr.arc(x + r, y + r, r, 2.0 * FRAC_PI_2, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// `>_`, the shell prompt.
fn prompt(cr: &cairo::Context, x: f64, y: f64, s: f64, width: f64) {
    cr.move_to(x + s * 0.1, y + s * 0.2);
    cr.line_to(x + s * 0.45, y + s * 0.5);
    cr.line_to(x + s * 0.1, y + s * 0.8);
    cr.move_to(x + s * 0.55, y + s * 0.82);
    cr.line_to(x + s * 0.95, y + s * 0.82);
    cr.set_line_width(width);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    let _ = cr.stroke();
}

/// A gauge: a dial with a needle.
fn gauge(cr: &cairo::Context, s: f64) {
    let c = s / 2.0;
    cr.set_line_width((s / 10.0).max(1.2));
    cr.set_line_cap(cairo::LineCap::Round);
    cr.arc(c, c, s * 0.42, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();
    cr.move_to(c, c);
    cr.line_to(c - s * 0.18, c - s * 0.16);
    let _ = cr.stroke();
    cr.arc(c, c, s * 0.07, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
}

/// A four-pointed sparkle.
fn sparkle(cr: &cairo::Context, x: f64, y: f64, s: f64) {
    let (cx, cy, r, k) = (x + s / 2.0, y + s / 2.0, s / 2.0, s * 0.08);
    cr.move_to(cx, cy - r);
    cr.curve_to(cx + k, cy - k, cx + k, cy - k, cx + r, cy);
    cr.curve_to(cx + k, cy + k, cx + k, cy + k, cx, cy + r);
    cr.curve_to(cx - k, cy + k, cx - k, cy + k, cx - r, cy);
    cr.curve_to(cx - k, cy - k, cx - k, cy - k, cx, cy - r);
    cr.close_path();
    let _ = cr.fill();
}

/// The agent's mark (24×24 SVG path), scaled into the square at (`x`, `y`).
fn logo(cr: &cairo::Context, icon: PaneIcon, x: f64, y: f64, s: f64) {
    let path = if icon == PaneIcon::Claude { CLAUDE_PATH } else { OPENAI_PATH };
    let k = s / 24.0;
    for cmd in parse(path) {
        match cmd {
            Cmd::Move(a, b) => cr.move_to(x + a * k, y + b * k),
            Cmd::Line(a, b) => cr.line_to(x + a * k, y + b * k),
            Cmd::Curve(a) => cr.curve_to(
                x + a[0] * k,
                y + a[1] * k,
                x + a[2] * k,
                y + a[3] * k,
                x + a[4] * k,
                y + a[5] * k,
            ),
            Cmd::Close => cr.close_path(),
        }
    }
    cr.set_fill_rule(cairo::FillRule::EvenOdd);
    let _ = cr.fill();
    cr.set_fill_rule(cairo::FillRule::Winding);
}

#[derive(Debug, PartialEq)]
enum Cmd {
    Move(f64, f64),
    Line(f64, f64),
    Curve([f64; 6]),
    Close,
}

/// Absolute M/L/C/Z path data, as the constants below hold it.
fn parse(d: &str) -> Vec<Cmd> {
    let mut out = Vec::new();
    let mut cmd = ' ';
    let mut nums: Vec<f64> = Vec::new();
    let flush = |cmd: char, nums: &mut Vec<f64>, out: &mut Vec<Cmd>| {
        match (cmd, nums.as_slice()) {
            ('M', [a, b]) => out.push(Cmd::Move(*a, *b)),
            ('L', [a, b]) => out.push(Cmd::Line(*a, *b)),
            ('C', [a, b, c, d, e, f]) => out.push(Cmd::Curve([*a, *b, *c, *d, *e, *f])),
            _ => {}
        }
        nums.clear();
    };
    let mut number = String::new();
    for ch in d.chars().chain(std::iter::once(' ')) {
        let starts_number = ch == '-' && !number.is_empty() && !number.ends_with('e');
        if ch.is_ascii_digit() || ch == '.' || (ch == '-' && !starts_number) {
            number.push(ch);
            continue;
        }
        if !number.is_empty() {
            if let Ok(v) = number.parse() {
                nums.push(v);
            }
            number.clear();
        }
        if starts_number {
            number.push(ch);
            continue;
        }
        if ch.is_ascii_alphabetic() {
            flush(cmd, &mut nums, &mut out);
            if ch == 'Z' {
                out.push(Cmd::Close);
            }
            cmd = ch;
        }
    }
    flush(cmd, &mut nums, &mut out);
    out
}

/// Claude's mark, from Simple Icons (CC0), arcs flattened to curves.
const CLAUDE_PATH: &str = "M4.714 15.956L9.432 13.308L9.511 13.078L9.432 12.95L9.201 12.95L8.412 12.902L5.716 12.829L3.379 12.732L1.114 12.61L0.543 12.489L0.009 11.785L0.064 11.432L0.543 11.111L1.229 11.171L2.747 11.275L5.024 11.432L6.675 11.53L9.122 11.785L9.511 11.785L9.565 11.627L9.432 11.53L9.329 11.432L6.973 9.836L4.423 8.148L3.087 7.176L2.365 6.684L2.001 6.223L1.843 5.215L2.498 4.493L3.379 4.553L3.603 4.614L4.496 5.3L6.402 6.776L8.892 8.609L9.256 8.913L9.402 8.809L9.42 8.737L9.256 8.463L7.902 6.017L6.457 3.527L5.813 2.495L5.643 1.876C5.583 1.621 5.54 1.409 5.54 1.147L6.287 0.134L6.7 0L7.695 0.134L8.114 0.498L8.734 1.913L9.735 4.141L11.29 7.17L11.745 8.069L11.988 8.901L12.079 9.156L12.237 9.156L12.237 9.01L12.364 7.304L12.601 5.209L12.832 2.514L12.911 1.755L13.287 0.844L14.034 0.352L14.617 0.631L15.096 1.317L15.03 1.761L14.744 3.612L14.186 6.515L13.821 8.457L14.034 8.457L14.277 8.215L15.26 6.909L16.912 4.845L17.64 4.025L18.49 3.121L19.037 2.69L20.069 2.69L20.828 3.819L20.488 4.985L19.425 6.332L18.545 7.474L17.282 9.174L16.493 10.534L16.566 10.643L16.754 10.625L19.607 10.018L21.149 9.738L22.989 9.423L23.821 9.811L23.912 10.206L23.584 11.013L21.617 11.499L19.31 11.961L15.874 12.774L15.831 12.805L15.88 12.865L17.428 13.011L18.09 13.047L19.711 13.047L22.728 13.272L23.517 13.794L23.991 14.432L23.912 14.917L22.698 15.537L21.058 15.148L17.233 14.237L15.922 13.909L15.74 13.909L15.74 14.019L16.833 15.087L18.836 16.897L21.344 19.228L21.471 19.805L21.15 20.26L20.81 20.212L18.606 18.554L17.756 17.807L15.831 16.186L15.704 16.186L15.704 16.356L16.147 17.006L18.49 20.527L18.612 21.608L18.442 21.96L17.835 22.173L17.167 22.051L15.795 20.127L14.38 17.959L13.239 16.016L13.099 16.095L12.425 23.35L12.109 23.721L11.381 24L10.774 23.539L10.452 22.792L10.774 21.316L11.162 19.392L11.478 17.862L11.763 15.961L11.933 15.33L11.921 15.288L11.781 15.306L10.349 17.273L8.169 20.218L6.445 22.063L6.032 22.227L5.316 21.857L5.382 21.195L5.783 20.606L8.169 17.57L9.608 15.688L10.537 14.602L10.531 14.444L10.476 14.444L4.138 18.56L3.008 18.706L2.523 18.25L2.583 17.504L2.814 17.261L4.721 15.949Z";

/// OpenAI's mark (Codex), from LobeHub Icons (MIT), arcs flattened to curves.
const OPENAI_PATH: &str = "M9.205 8.658L9.205 6.398C9.205 6.208 9.277 6.065 9.443 5.97L13.986 3.354C14.605 2.997 15.342 2.831 16.103 2.831C18.957 2.831 20.765 5.043 20.765 7.397C20.765 7.564 20.765 7.754 20.741 7.944L16.031 5.185C15.903 5.104 15.755 5.06 15.603 5.06C15.451 5.06 15.303 5.104 15.175 5.185L9.205 8.658ZM19.814 17.458L19.814 12.06C19.814 11.727 19.671 11.49 19.385 11.323L13.415 7.85L15.365 6.732C15.436 6.686 15.518 6.661 15.603 6.661C15.688 6.661 15.77 6.686 15.841 6.732L20.384 9.349C21.693 10.109 22.573 11.727 22.573 13.297C22.573 15.105 21.503 16.77 19.813 17.46ZM7.802 12.703L5.852 11.561C5.685 11.466 5.613 11.323 5.613 11.133L5.613 5.899C5.613 3.354 7.563 1.427 10.204 1.427C11.204 1.427 12.131 1.76 12.916 2.355L8.23 5.067C7.945 5.233 7.802 5.471 7.802 5.804L7.802 12.702ZM12 15.128L9.205 13.558L9.205 10.228L12 8.658L14.795 10.228L14.795 13.558L12 15.128ZM13.796 22.358C12.796 22.358 11.869 22.026 11.084 21.431L15.77 18.719C16.055 18.553 16.198 18.315 16.198 17.982L16.198 11.084L18.172 12.226C18.339 12.321 18.41 12.464 18.41 12.654L18.41 17.887C18.41 20.432 16.436 22.359 13.796 22.359ZM8.159 17.055L3.615 14.438C2.307 13.677 1.427 12.06 1.427 10.49C1.424 9.598 1.687 8.725 2.183 7.984C2.679 7.242 3.385 6.665 4.21 6.327L4.21 11.75C4.21 12.083 4.353 12.321 4.638 12.488L10.585 15.937L8.635 17.055C8.564 17.102 8.482 17.126 8.397 17.126C8.312 17.126 8.23 17.102 8.159 17.055ZM7.897 20.955C5.209 20.955 3.235 18.934 3.235 16.436C3.235 16.246 3.259 16.056 3.282 15.866L7.968 18.576C8.254 18.743 8.539 18.743 8.824 18.576L14.794 15.128L14.794 17.388C14.794 17.578 14.724 17.721 14.557 17.816L10.014 20.432C9.395 20.789 8.658 20.955 7.897 20.955ZM13.796 23.785C14.705 23.785 15.601 23.577 16.417 23.176C17.233 22.776 17.946 22.194 18.502 21.475C19.057 20.756 19.441 19.919 19.623 19.029C22.287 18.339 24 15.84 24 13.296C24 11.631 23.287 10.014 22.002 8.848C22.121 8.348 22.192 7.849 22.192 7.35C22.192 3.949 19.433 1.403 16.246 1.403C15.604 1.403 14.986 1.498 14.366 1.713C13.816 1.172 13.167 0.743 12.453 0.449C11.74 0.155 10.976 0.003 10.205 0C9.296 -0 8.399 0.208 7.584 0.609C6.768 1.009 6.055 1.591 5.499 2.311C4.943 3.03 4.56 3.867 4.378 4.757C1.713 5.447 0 7.945 0 10.49C0 12.156 0.713 13.773 1.998 14.938C1.879 15.438 1.808 15.938 1.808 16.437C1.808 19.838 4.567 22.383 7.754 22.383C8.396 22.383 9.014 22.288 9.634 22.074C10.184 22.615 10.834 23.045 11.547 23.338C12.261 23.632 13.024 23.784 13.796 23.787Z";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse("M1 2L-3.5 4C1 2 3 4 5 6Z"),
            vec![
                Cmd::Move(1.0, 2.0),
                Cmd::Line(-3.5, 4.0),
                Cmd::Curve([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
                Cmd::Close
            ]
        );
    }

    #[test]
    fn logos_stay_in_their_box() {
        for path in [CLAUDE_PATH, OPENAI_PATH] {
            let cmds = parse(path);
            assert!(cmds.len() > 20);
            for c in cmds {
                let pts: Vec<f64> = match c {
                    Cmd::Move(a, b) | Cmd::Line(a, b) => vec![a, b],
                    Cmd::Curve(a) => a.to_vec(),
                    Cmd::Close => vec![],
                };
                assert!(pts.iter().all(|v| (-0.5..=24.5).contains(v)), "{pts:?}");
            }
        }
    }
}
