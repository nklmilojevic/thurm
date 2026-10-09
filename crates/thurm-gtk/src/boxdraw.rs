//! Procedural box drawing (U+2500–U+257F) and block elements (U+2580–U+259F), so lines connect
//! across cells whatever the font. A port of BoxDrawing.swift.

/// A solid rectangle inside a cell, in pixels from the cell's top-left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub alpha: f64,
    /// A quarter-circle stroke inside this rect (rounded corners): radius, thickness and the
    /// circle center relative to the rect.
    pub arc: Option<(f64, f64, f64, f64)>,
}

fn rect(x: i32, y: i32, w: i32, h: i32, alpha: f64) -> BoxRect {
    BoxRect {
        x,
        y,
        w: w.max(0),
        h: h.max(0),
        alpha,
        arc: None,
    }
}

/// Arm weights per character, "URDL": 0 none, 1 light, 2 heavy, 3 double; "-" is left to the
/// font (diagonals).
const TABLE: &str = "\
0101 0202 1010 2020 - - - - - - - - \
0110 0210 0120 0220 0011 0012 0021 0022 1100 1200 2100 2200 1001 1002 2001 2002 \
1110 1210 2110 1120 2120 2210 1220 2220 1011 1012 2011 1021 2021 2012 1022 2022 \
0111 0112 0211 0212 0121 0122 0221 0222 1101 1102 1201 1202 2101 2102 2201 2202 \
1111 1112 1211 1212 2111 1121 2121 2112 2211 1122 1221 2212 1222 2122 2221 2222 \
- - - - \
0303 3030 0310 0130 0330 0013 0031 0033 1300 3100 3300 1003 3001 3003 1310 3130 \
3330 1013 3031 3033 0313 0131 0333 1303 3101 3303 1313 3131 3333 \
0110 0011 1001 1100 - - - \
0001 1000 0100 0010 0002 2000 0200 0020 0201 1020 0102 2010";

fn arms(ch: u32) -> Option<(i32, i32, i32, i32)> {
    if !(0x2500..=0x257F).contains(&ch) {
        return None;
    }
    let entry = TABLE.split(' ').nth((ch - 0x2500) as usize)?;
    let d: Vec<i32> = entry
        .chars()
        .filter_map(|c| c.to_digit(10))
        .map(|d| d as i32)
        .collect();
    (d.len() == 4).then(|| (d[0], d[1], d[2], d[3]))
}

/// Dashed lines: (horizontal, heavy, dashes).
fn dashed(ch: u32) -> Option<(bool, bool, i32)> {
    Some(match ch {
        0x2504 => (true, false, 3),
        0x2505 => (true, true, 3),
        0x2506 => (false, false, 3),
        0x2507 => (false, true, 3),
        0x2508 => (true, false, 4),
        0x2509 => (true, true, 4),
        0x250A => (false, false, 4),
        0x250B => (false, true, 4),
        0x254C => (true, false, 2),
        0x254D => (true, true, 2),
        0x254E => (false, false, 2),
        0x254F => (false, true, 2),
        _ => return None,
    })
}

/// Whether `ch` is drawn here rather than by the font.
pub fn is_procedural(ch: u32) -> bool {
    (0x2580..=0x259F).contains(&ch)
        || dashed(ch).is_some()
        || (0x256D..=0x2570).contains(&ch)
        || arms(ch).is_some()
}

/// Rectangles for `ch` in a `w`×`h` cell; `light` is the light line thickness.
pub fn rects(ch: u32, w: i32, h: i32, light: i32) -> Vec<BoxRect> {
    if (0x2580..=0x259F).contains(&ch) {
        return block_rects(ch, w, h);
    }
    if let Some((horizontal, heavy, n)) = dashed(ch) {
        return dash_rects(horizontal, heavy, n, w, h, light);
    }
    if (0x256D..=0x2570).contains(&ch) {
        return rounded_corner(ch, w, h, light);
    }
    match arms(ch) {
        Some((u, r, d, l)) => line_rects(u, r, d, l, w, h, light),
        None => Vec::new(),
    }
}

fn light_width(w: i32, h: i32, light: i32) -> i32 {
    light.min((w.min(h) / 5).max(1)).max(1)
}

fn line_width(w: i32, h: i32, light: i32, heavy: bool) -> i32 {
    let l = light_width(w, h, light);
    if heavy { (l + 1).max(l * 2) } else { l }
}

#[allow(clippy::too_many_arguments)]
fn line_rects(
    up: i32,
    right: i32,
    down: i32,
    left: i32,
    w: i32,
    h: i32,
    light: i32,
) -> Vec<BoxRect> {
    let l = light_width(w, h, light);
    let heavy = (l + 1).max(l * 2);
    let span = |weight: i32| match weight {
        1 => l,
        2 => heavy,
        3 => 3 * l,
        _ => 0,
    };
    let cx = w / 2;
    let cy = h / 2;
    let start = |c: i32, weight: i32| c - span(weight) / 2;
    let v_span = span(up).max(span(down));
    let h_span = span(left).max(span(right));
    let v_start = cx - v_span / 2;
    let h_start = cy - h_span / 2;
    let full_end_x = if v_span > 0 { v_start + v_span } else { cx + 1 };
    let full_start_x = if v_span > 0 { v_start } else { cx };
    let full_end_y = if h_span > 0 { h_start + h_span } else { cy + 1 };
    let full_start_y = if h_span > 0 { h_start } else { cy };
    let inner_end_x = |p: i32| {
        if p == 3 {
            start(cx, 3) + l
        } else {
            start(cx, p) + span(p)
        }
    };
    let inner_start_x = |p: i32| {
        if p == 3 {
            start(cx, 3) + 2 * l
        } else {
            start(cx, p)
        }
    };
    let inner_end_y = |p: i32| {
        if p == 3 {
            start(cy, 3) + l
        } else {
            start(cy, p) + span(p)
        }
    };
    let inner_start_y = |p: i32| {
        if p == 3 {
            start(cy, 3) + 2 * l
        } else {
            start(cy, p)
        }
    };

    let mut out = Vec::new();
    let mut add = |x0: i32, y0: i32, x1: i32, y1: i32| {
        let x = x0.min(x1).max(0);
        let y = y0.min(y1).max(0);
        let xe = x0.max(x1).min(w);
        let ye = y0.max(y1).min(h);
        if xe > x && ye > y {
            out.push(rect(x, y, xe - x, ye - y, 1.0));
        }
    };

    if left == 1 || left == 2 {
        let end = if up == 3 && down == 3 && right == 0 {
            v_start + l
        } else {
            full_end_x
        };
        let y = start(cy, left);
        add(0, y, end, y + span(left));
    } else if left == 3 {
        let s = start(cy, 3);
        add(
            0,
            s,
            if up > 0 { inner_end_x(up) } else { full_end_x },
            s + l,
        );
        add(
            0,
            s + 2 * l,
            if down > 0 {
                inner_end_x(down)
            } else {
                full_end_x
            },
            s + 3 * l,
        );
    }
    if right == 1 || right == 2 {
        let begin = if up == 3 && down == 3 && left == 0 {
            v_start + 2 * l
        } else {
            full_start_x
        };
        let y = start(cy, right);
        add(begin, y, w, y + span(right));
    } else if right == 3 {
        let s = start(cy, 3);
        add(
            if up > 0 {
                inner_start_x(up)
            } else {
                full_start_x
            },
            s,
            w,
            s + l,
        );
        add(
            if down > 0 {
                inner_start_x(down)
            } else {
                full_start_x
            },
            s + 2 * l,
            w,
            s + 3 * l,
        );
    }
    if up == 1 || up == 2 {
        let end = if left == 3 && right == 3 && down == 0 {
            h_start + l
        } else {
            full_end_y
        };
        let x = start(cx, up);
        add(x, 0, x + span(up), end);
    } else if up == 3 {
        let s = start(cx, 3);
        add(
            s,
            0,
            s + l,
            if left > 0 {
                inner_end_y(left)
            } else {
                full_end_y
            },
        );
        add(
            s + 2 * l,
            0,
            s + 3 * l,
            if right > 0 {
                inner_end_y(right)
            } else {
                full_end_y
            },
        );
    }
    if down == 1 || down == 2 {
        let begin = if left == 3 && right == 3 && up == 0 {
            h_start + 2 * l
        } else {
            full_start_y
        };
        let x = start(cx, down);
        add(x, begin, x + span(down), h);
    } else if down == 3 {
        let s = start(cx, 3);
        add(
            s,
            if left > 0 {
                inner_start_y(left)
            } else {
                full_start_y
            },
            s + l,
            h,
        );
        add(
            s + 2 * l,
            if right > 0 {
                inner_start_y(right)
            } else {
                full_start_y
            },
            s + 3 * l,
            h,
        );
    }
    out
}

/// `count` dashes per cell, the gap split across both ends so neighbouring cells tile evenly.
fn dash_rects(
    horizontal: bool,
    heavy: bool,
    count: i32,
    w: i32,
    h: i32,
    light: i32,
) -> Vec<BoxRect> {
    let t = line_width(w, h, light, heavy);
    let length = if horizontal { w } else { h };
    let mut out = Vec::new();
    for i in 0..count {
        let a = length * i / count;
        let b = length * (i + 1) / count;
        let gap = ((b - a) / 3).max(1);
        let s = a + gap / 2;
        let e = b - (gap - gap / 2);
        if e <= s {
            continue;
        }
        if horizontal {
            out.push(rect(s, h / 2 - t / 2, e - s, t, 1.0));
        } else {
            out.push(rect(w / 2 - t / 2, s, t, e - s, 1.0));
        }
    }
    out
}

/// ╭ ╮ ╯ ╰: a quarter circle through the cell center with straight arms to the edges.
fn rounded_corner(ch: u32, w: i32, h: i32, light: i32) -> Vec<BoxRect> {
    let t = line_width(w, h, light, false);
    let cx = w / 2;
    let cy = h / 2;
    let lx = (cx - t / 2) as f64 + t as f64 / 2.0;
    let ly = (cy - t / 2) as f64 + t as f64 / 2.0;
    let r = w.min(h) as f64 / 2.0;
    let right = ch == 0x256D || ch == 0x2570;
    let down = ch == 0x256D || ch == 0x256E;
    let ccx = if right { lx + r } else { lx - r };
    let ccy = if down { ly + r } else { ly - r };
    let half = t as f64 / 2.0 + 1.0;
    let (qx0, qx1) = if right {
        (lx - half, ccx)
    } else {
        (ccx, lx + half)
    };
    let (qy0, qy1) = if down {
        (ly - half, ccy)
    } else {
        (ccy, ly + half)
    };
    let x0 = qx0.floor() as i32;
    let y0 = qy0.floor() as i32;
    let x1 = qx1.ceil() as i32;
    let y1 = qy1.ceil() as i32;
    let mut out = vec![BoxRect {
        arc: Some((r, t as f64, ccx - x0 as f64, ccy - y0 as f64)),
        ..rect(x0, y0, x1 - x0, y1 - y0, 1.0)
    }];
    let hy = cy - t / 2;
    let vx = cx - t / 2;
    if right {
        let s = ccx.round() as i32;
        if s < w {
            out.push(rect(s, hy, w - s, t, 1.0));
        }
    } else {
        let e = ccx.round() as i32;
        if e > 0 {
            out.push(rect(0, hy, e, t, 1.0));
        }
    }
    if down {
        let s = ccy.round() as i32;
        if s < h {
            out.push(rect(vx, s, t, h - s, 1.0));
        }
    } else {
        let e = ccy.round() as i32;
        if e > 0 {
            out.push(rect(vx, 0, t, e, 1.0));
        }
    }
    out
}

fn block_rects(ch: u32, w: i32, h: i32) -> Vec<BoxRect> {
    let eighths_y = |n: i32| (h * n + 4) / 8;
    let eighths_x = |n: i32| (w * n + 4) / 8;
    let hx = (w + 1) / 2;
    let hy = (h + 1) / 2;
    let quadrants = |mask: i32| {
        let mut out = Vec::new();
        if mask & 1 != 0 {
            out.push(rect(0, 0, hx, hy, 1.0));
        }
        if mask & 2 != 0 {
            out.push(rect(hx, 0, w - hx, hy, 1.0));
        }
        if mask & 4 != 0 {
            out.push(rect(0, hy, hx, h - hy, 1.0));
        }
        if mask & 8 != 0 {
            out.push(rect(hx, hy, w - hx, h - hy, 1.0));
        }
        out
    };
    match ch {
        0x2580 => vec![rect(0, 0, w, hy, 1.0)],
        0x2581..=0x2587 => {
            let eh = eighths_y((ch - 0x2580) as i32);
            vec![rect(0, h - eh, w, eh, 1.0)]
        }
        0x2588 => vec![rect(0, 0, w, h, 1.0)],
        0x2589..=0x258F => vec![rect(0, 0, eighths_x((0x2590 - ch) as i32), h, 1.0)],
        0x2590 => vec![rect(hx, 0, w - hx, h, 1.0)],
        0x2591 => vec![rect(0, 0, w, h, 0.25)],
        0x2592 => vec![rect(0, 0, w, h, 0.5)],
        0x2593 => vec![rect(0, 0, w, h, 0.75)],
        0x2594 => vec![rect(0, 0, w, eighths_y(1).max(1), 1.0)],
        0x2595 => {
            let ew = eighths_x(1).max(1);
            vec![rect(w - ew, 0, ew, h, 1.0)]
        }
        0x2596 => quadrants(4),
        0x2597 => quadrants(8),
        0x2598 => quadrants(1),
        0x2599 => quadrants(1 | 4 | 8),
        0x259A => quadrants(1 | 8),
        0x259B => quadrants(1 | 2 | 4),
        0x259C => quadrants(1 | 2 | 8),
        0x259D => quadrants(2),
        0x259E => quadrants(2 | 4),
        0x259F => quadrants(2 | 4 | 8),
        _ => Vec::new(),
    }
}

/// Paints `ch` into the cell at (x, y) in `color` (0xRRGGBB).
#[allow(clippy::too_many_arguments)]
pub fn paint(
    cr: &gtk::cairo::Context,
    ch: u32,
    x: f64,
    y: f64,
    w: i32,
    h: i32,
    light: i32,
    color: u32,
) {
    let (r, g, b, a) = (
        ((color >> 16) & 0xff) as f64 / 255.0,
        ((color >> 8) & 0xff) as f64 / 255.0,
        (color & 0xff) as f64 / 255.0,
        1.0,
    );
    for br in rects(ch, w, h, light) {
        cr.set_source_rgba(r, g, b, a * br.alpha);
        match br.arc {
            Some((radius, thickness, acx, acy)) => {
                let _ = cr.save();
                cr.rectangle(x + br.x as f64, y + br.y as f64, br.w as f64, br.h as f64);
                cr.clip();
                cr.set_line_width(thickness);
                cr.arc(
                    x + br.x as f64 + acx,
                    y + br.y as f64 + acy,
                    radius,
                    0.0,
                    std::f64::consts::TAU,
                );
                let _ = cr.stroke();
                let _ = cr.restore();
            }
            None => {
                cr.rectangle(x + br.x as f64, y + br.y as f64, br.w as f64, br.h as f64);
                let _ = cr.fill();
            }
        }
    }
    cr.set_source_rgba(r, g, b, a);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_line_spans_the_cell() {
        let r = rects(0x2500, 10, 20, 1);
        assert_eq!(r.len(), 2);
        assert_eq!(r.iter().map(|r| r.w).sum::<i32>(), 10 + 1);
        assert!(r.iter().all(|r| r.h == 1 && r.y == 10));
    }

    #[test]
    fn cross_has_four_arms() {
        assert_eq!(rects(0x253C, 10, 20, 1).len(), 4);
    }

    #[test]
    fn full_block_and_shades() {
        assert_eq!(rects(0x2588, 9, 18, 1), vec![rect(0, 0, 9, 18, 1.0)]);
        assert_eq!(rects(0x2592, 9, 18, 1)[0].alpha, 0.5);
        assert_eq!(rects(0x2584, 9, 18, 1), vec![rect(0, 9, 9, 9, 1.0)]);
    }

    #[test]
    fn procedural_set() {
        assert!(is_procedural(0x2502));
        assert!(is_procedural(0x256D));
        assert!(is_procedural(0x2504));
        assert!(!is_procedural(0x2571)); // diagonal: the font's
        assert!(!is_procedural('a' as u32));
    }

    #[test]
    fn rounded_corner_has_an_arc() {
        let r = rects(0x256D, 10, 20, 1);
        assert!(r[0].arc.is_some());
        // The arc (radius: half the cell) reaches the right edge; the down arm is left.
        assert_eq!(r.len(), 2);
    }
}
