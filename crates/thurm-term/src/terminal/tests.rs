use super::*;
use thurm_proto::{Key, KeyAction, NamedKey};

fn term(cols: u16, rows: u16) -> Terminal {
    Terminal::new(
        PaneSize {
            cols,
            rows,
            cell_width: 10,
            cell_height: 20,
        },
        EngineConfig::default(),
    )
}

fn row_text(f: &Frame, row: u16) -> String {
    f.lines
        .iter()
        .find(|l| l.row == row)
        .map(|l| {
            l.cells
                .iter()
                .filter(|c| c.flags & cf::WIDE_SPACER == 0)
                .map(|c| c.ch)
                .collect::<String>()
        })
        .unwrap_or_default()
        .trim_end()
        .to_owned()
}

#[test]
fn full_then_incremental_frames() {
    let mut t = term(20, 5);
    let mut view = ClientView::new();
    t.advance(b"hello\r\nworld");
    let s = t.snapshot(1, &mut view).expect("first frame");
    assert!(s.frame.full);
    assert_eq!(s.frame.lines.len(), 5);
    assert_eq!(row_text(&s.frame, 0), "hello");
    assert_eq!(row_text(&s.frame, 1), "world");
    assert_eq!(s.frame.cursor.row, 1);
    assert_eq!(s.frame.cursor.col, 5);

    // Nothing changed: no frame.
    assert!(t.snapshot(1, &mut view).is_none());

    t.advance(b"!");
    let s = t.snapshot(1, &mut view).expect("delta");
    assert!(!s.frame.full);
    assert_eq!(s.frame.lines.len(), 1);
    assert_eq!(s.frame.lines[0].row, 1);
    assert_eq!(row_text(&s.frame, 1), "world!");
}

#[test]
fn colors_and_attributes() {
    let mut t = term(20, 2);
    let mut view = ClientView::new();
    t.advance(b"\x1b[1;31mR\x1b[0m\x1b[38;2;1;2;3mT\x1b[7mI\x1b[0m\x1b[4:3;58;2;9;8;7mC\x1b[0m");
    let s = t.snapshot(1, &mut view).unwrap();
    let cells = &s.frame.lines[0].cells;
    let theme = thurm_config::builtin_theme("thurm").unwrap();
    assert_eq!(cells[0].fg, theme.palette[1]);
    assert!(cells[0].flags & cf::BOLD != 0);
    assert_eq!(cells[1].fg, 0x010203);
    assert!(cells[1].flags & cf::DEFAULT_BG != 0);
    // Inverse swaps fg and bg.
    assert_eq!(cells[2].bg, 0x010203);
    assert!(cells[2].flags & cf::DEFAULT_BG == 0);
    assert!(cells[3].flags & cf::UNDERCURL != 0);
    assert_eq!(cells[3].ul, 0x090807);
    assert_eq!(cells[4].ul, proto::NO_COLOR);
}

#[test]
fn osc_palette_override_and_query() {
    let mut t = term(10, 2);
    t.advance(b"\x1b]4;1;rgb:ff/00/00\x07\x1b[31mX");
    let mut view = ClientView::new();
    let s = t.snapshot(1, &mut view).unwrap();
    assert_eq!(s.frame.lines[0].cells[0].fg, 0xff0000);
    t.advance(b"\x1b]11;?\x07");
    let ev = t.drain_events();
    assert!(
        ev.iter()
            .any(|e| matches!(e, TermEvent::PtyWrite(b) if b.starts_with(b"\x1b]11;rgb:"))),
        "{ev:?}"
    );
}

#[test]
fn wide_chars_and_clusters() {
    let mut t = term(10, 2);
    t.advance("日e\u{301}".as_bytes());
    let mut view = ClientView::new();
    let s = t.snapshot(1, &mut view).unwrap();
    let row = &s.frame.lines[0];
    assert!(row.cells[0].flags & cf::WIDE != 0);
    assert!(row.cells[1].flags & cf::WIDE_SPACER != 0);
    assert_eq!(row.clusters, vec![(2, "e\u{301}".to_string())]);
}

#[test]
fn hyperlinks_get_stable_ids() {
    let mut t = term(20, 2);
    t.advance(b"\x1b]8;;https://a.example\x1b\\A\x1b]8;;\x1b\\ \x1b]8;;https://b.example\x1b\\B\x1b]8;;\x1b\\");
    let mut view = ClientView::new();
    let s = t.snapshot(1, &mut view).unwrap();
    assert_eq!(
        s.frame.links,
        vec!["https://a.example", "https://b.example"]
    );
    let cells = &s.frame.lines[0].cells;
    assert_eq!(cells[0].link, 1);
    assert_eq!(cells[1].link, 0);
    assert_eq!(cells[2].link, 2);
    t.advance(b"x");
    let s = t.snapshot(1, &mut view).unwrap();
    assert!(
        s.frame.links.is_empty(),
        "unchanged link table is not resent"
    );
}

#[test]
fn link_table_starts_over_when_full() {
    // Screens of distinct links, one after another: past MAX_LINKS the table holds only
    // what is on screen, and the client gets it again with the rows that use it.
    let (cols, rows) = (40u16, 25u16);
    let mut t = term(cols, rows);
    let screen = |t: &mut Terminal, tag: usize| {
        let mut out = String::from("\x1b[H\x1b[2J");
        for r in 0..rows {
            for c in 0..cols / 2 {
                out.push_str(&format!(
                    "\x1b]8;;https://{tag}/{r}/{c}\x1b\\x\x1b]8;;\x1b\\ "
                ));
            }
            if r + 1 < rows {
                out.push_str("\r\n");
            }
        }
        t.advance(out.as_bytes());
    };
    let per_screen = (cols / 2) as usize * rows as usize;
    let screens = MAX_LINKS / per_screen + 1;
    let mut view = ClientView::new();
    for tag in 0..screens {
        screen(&mut t, tag);
        let s = t.snapshot(1, &mut view).unwrap();
        assert_eq!(s.frame.links.len(), (tag + 1) * per_screen);
    }
    t.advance(b"\x1b[H");
    let s = t.snapshot(1, &mut view).unwrap();
    let last = screens - 1;
    assert_eq!(s.frame.links.len(), per_screen);
    assert!(
        s.frame
            .links
            .iter()
            .all(|l| l.starts_with(&format!("https://{last}/")))
    );
    assert_eq!(
        s.frame.lines.len(),
        rows as usize,
        "every row is sent with the new ids"
    );
    let first = s.frame.lines.iter().find(|l| l.row == 0).unwrap().cells[0].link;
    assert_eq!(
        s.frame.links[first as usize - 1],
        format!("https://{last}/0/0")
    );
}

#[test]
fn intercepted_osc_events() {
    let mut t = term(20, 2);
    t.advance(
        b"\x1b]7;file://host/tmp/x%20y\x07\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07",
    );
    t.advance(b"\x1b]9;done\x07\x1b]133;D;3\x07\x1b]0;my title\x07");
    let ev = t.drain_events();
    assert!(ev.contains(&TermEvent::Cwd("/tmp/x y".into())));
    assert!(ev.contains(&TermEvent::PromptStart));
    assert!(ev.contains(&TermEvent::CommandStart));
    assert!(ev.contains(&TermEvent::CommandFinished(Some(3))));
    assert!(ev.contains(&TermEvent::Notify {
        title: String::new(),
        body: "done".into()
    }));
    assert!(ev.contains(&TermEvent::Title(Some("my title".into()))));
    assert_eq!(t.title(), Some("my title"));
}

#[test]
fn prompt_jumping() {
    let mut t = term(20, 3);
    for i in 0..3 {
        t.advance(format!("\x1b]133;A\x07$ \x1b]133;B\x07cmd{i}\r\n").as_bytes());
        for j in 0..5 {
            t.advance(format!("out{i}.{j}\r\n").as_bytes());
        }
    }
    assert_eq!(t.prompt_lines().len(), 3);
    t.scroll(ScrollCmd::PrevPrompt);
    let mut view = ClientView::new();
    let s = t.snapshot(1, &mut view).unwrap();
    assert!(
        row_text(&s.frame, 0).starts_with("$ cmd2"),
        "{:?}",
        row_text(&s.frame, 0)
    );
    t.scroll(ScrollCmd::PrevPrompt);
    let s = t.snapshot(1, &mut view).unwrap();
    assert!(row_text(&s.frame, 0).starts_with("$ cmd1"));
    t.scroll(ScrollCmd::NextPrompt);
    let s = t.snapshot(1, &mut view).unwrap();
    assert!(row_text(&s.frame, 0).starts_with("$ cmd2"));
}

#[test]
fn last_command_output() {
    let mut t = term(20, 4);
    assert_eq!(t.last_command(50), None);
    for i in 0..2 {
        t.advance(format!("\x1b]133;A\x07$ \x1b]133;B\x07cmd{i}\r\n").as_bytes());
        for j in 0..5 {
            t.advance(format!("out{i}.{j}\r\n").as_bytes());
        }
    }
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(
        t.last_command(50).as_deref(),
        Some("$ cmd1\nout1.0\nout1.1\nout1.2\nout1.3\nout1.4")
    );
    assert_eq!(t.last_command(2).as_deref(), Some("out1.3\nout1.4"));
}

#[test]
fn selection_and_copy() {
    let mut t = term(20, 3);
    t.advance(b"foo bar baz\r\nline two");
    t.selection(SelectionOp::Start {
        col: 4,
        row: 0,
        right_half: false,
        kind: SelectionKind::Word,
    });
    assert_eq!(t.selection_text(), "bar");
    t.selection(SelectionOp::Start {
        col: 0,
        row: 1,
        right_half: false,
        kind: SelectionKind::Line,
    });
    assert_eq!(t.selection_text().trim_end(), "line two");
    let mut view = ClientView::new();
    let s = t.snapshot(1, &mut view).unwrap();
    assert!(s.frame.lines[1].cells[0].flags & cf::SELECTED != 0);
    t.selection(SelectionOp::Clear);
    assert_eq!(t.selection_text(), "");
}

#[test]
fn mouse_selection_vs_reporting() {
    let mut t = term(20, 3);
    t.advance(b"hello world");
    let ev = |kind, col, clicks, mods| MouseEvent {
        kind,
        button: MouseButton::Left,
        col,
        row: 0,
        right_half: false,
        mods,
        clicks,
        x: 0,
        y: 0,
    };
    assert_eq!(
        t.mouse(&ev(MouseKind::Press, 0, 1, 0)).0,
        MouseOutcome::SelectionChanged
    );
    let mut drag = ev(MouseKind::Move, 4, 1, 0);
    drag.right_half = true;
    t.mouse(&drag);
    assert_eq!(
        t.mouse(&ev(MouseKind::Release, 4, 1, 0)).0,
        MouseOutcome::SelectionDone
    );
    assert_eq!(t.selection_text(), "hello");

    // App grabs the mouse (SGR): clicks are reported unless Shift is held.
    t.advance(b"\x1b[?1000h\x1b[?1006h");
    let (o, bytes) = t.mouse(&ev(MouseKind::Press, 2, 1, 0));
    assert_eq!(o, MouseOutcome::Reported);
    assert_eq!(bytes, b"\x1b[<0;3;1M");
    assert_eq!(
        t.mouse(&ev(MouseKind::Press, 2, 1, mods::SHIFT)).0,
        MouseOutcome::SelectionChanged
    );
}

#[test]
fn keys_scroll_to_bottom_and_paste() {
    let mut t = term(10, 3);
    for i in 0..20 {
        t.advance(format!("{i}\r\n").as_bytes());
    }
    t.scroll(ScrollCmd::PageUp);
    assert!(t.display_offset() > 0);
    let bytes = t.key(&KeyEvent {
        key: Key::Char('a'),
        mods: 0,
        action: KeyAction::Press,
        text: "a".into(),
        shifted: None,
        base_layout: None,
    });
    assert_eq!(bytes, b"a");
    assert_eq!(t.display_offset(), 0);
    assert_eq!(t.paste("a\nb"), b"a\rb");
    t.advance(b"\x1b[?2004h");
    // The ESC of an embedded end marker becomes a space (libghostty-vt's paste encoding).
    assert_eq!(t.paste("x\x1b[201~y"), b"\x1b[200~x [201~y\x1b[201~");
    let enter = t.key(&KeyEvent {
        key: Key::Named(NamedKey::Enter),
        mods: 0,
        action: KeyAction::Press,
        text: "\r".into(),
        shifted: None,
        base_layout: None,
    });
    assert_eq!(enter, b"\r");
}

#[test]
fn search_finds_and_highlights() {
    let mut t = term(20, 3);
    for i in 0..10 {
        t.advance(format!("line {i}\r\n").as_bytes());
    }
    t.advance(b"call needle(x)");
    assert!(t.search(Some("line 2"), SearchDirection::Backward));
    let mut view = ClientView::new();
    let s = t.snapshot(1, &mut view).unwrap();
    let hit = s
        .frame
        .lines
        .iter()
        .any(|l| l.cells.iter().any(|c| c.flags & cf::SEARCH_FOCUS != 0));
    assert!(hit);
    assert!(!t.search(Some("zzz"), SearchDirection::Backward));
    assert!(!t.search(None, SearchDirection::Backward));
    // Invalid regexes are searched literally.
    assert!(t.search(Some("needle("), SearchDirection::Backward));
}

#[test]
fn capture_and_history_roundtrip() {
    let mut t = term(12, 4);
    t.advance(b"\x1b[32mgreen\x1b[0m plain\r\n");
    for i in 0..6 {
        t.advance(format!("row {i}\r\n").as_bytes());
    }
    t.advance(b"a very long line that wraps");
    let plain = t.capture(&CaptureOpts {
        lines: None,
        scrollback: true,
        ansi: false,
    });
    assert!(plain.starts_with("green plain\n"), "{plain:?}");
    assert!(plain.contains("row 5"));

    let screen = t.capture(&CaptureOpts::default());
    assert!(!screen.contains("green"));

    let hist = t.serialize_history(100).unwrap();
    let text = String::from_utf8_lossy(&hist);
    assert!(text.contains("\x1b[38;5;2mgreen"), "{text:?}");

    // Replaying into a fresh terminal reproduces the text, with soft wraps rejoined.
    let mut u = term(40, 10);
    u.replay(&hist);
    let back = u.capture(&CaptureOpts {
        lines: None,
        scrollback: true,
        ansi: false,
    });
    assert!(back.contains("green plain"));
    assert!(back.contains("a very long line that wraps"), "{back:?}");
    assert!(u.drain_events().is_empty());
}

#[test]
fn alt_screen_blocks_history_serialization() {
    let mut t = term(10, 3);
    t.advance(b"shell\r\n\x1b[?1049hvim");
    assert!(t.serialize_history(10).is_none());
    t.advance(b"\x1b[?1049l");
    assert!(String::from_utf8_lossy(&t.serialize_history(10).unwrap()).contains("shell"));
}

#[test]
fn resize_forces_full_frame() {
    let mut t = term(10, 3);
    let mut view = ClientView::new();
    t.snapshot(1, &mut view).unwrap();
    t.resize(PaneSize {
        cols: 20,
        rows: 5,
        cell_width: 10,
        cell_height: 20,
    });
    let s = t.snapshot(1, &mut view).unwrap();
    assert!(s.frame.full);
    assert_eq!((s.frame.cols, s.frame.rows), (20, 5));
}

#[test]
fn clipboard_osc52() {
    let mut t = term(10, 3);
    t.advance(b"\x1b]52;c;aGVsbG8=\x07");
    assert!(
        t.drain_events()
            .contains(&TermEvent::ClipboardStore("hello".into()))
    );
    // Copying can be turned off.
    t.set_config(EngineConfig {
        osc52: Osc52Mode::Disabled,
        ..EngineConfig::default()
    });
    t.drain_events();
    t.advance(b"\x1b]52;c;aGVsbG8=\x07");
    assert!(t.drain_events().is_empty());
    // A read is Thurm's (answered when the app has the clipboard), not answered empty.
    t.set_config(EngineConfig {
        osc52: Osc52Mode::CopyPaste,
        ..EngineConfig::default()
    });
    t.drain_events();
    t.advance(b"\x1b]52;c;?\x07");
    assert_eq!(t.drain_events(), vec![TermEvent::ClipboardLoad]);
}

#[test]
fn kitty_keyboard_mode_is_tracked() {
    let mut t = term(10, 3);
    t.advance(b"\x1b[>1u");
    assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
    let esc = t.key(&KeyEvent {
        key: Key::Named(NamedKey::Escape),
        mods: 0,
        action: KeyAction::Press,
        text: String::new(),
        shifted: None,
        base_layout: None,
    });
    assert_eq!(esc, b"\x1b[27u");
    // Query reports the flags.
    t.advance(b"\x1b[?u");
    assert!(
        t.drain_events()
            .iter()
            .any(|e| matches!(e, TermEvent::PtyWrite(b) if b == b"\x1b[?1u"))
    );
}

fn press(key: NamedKey) -> KeyEvent {
    KeyEvent {
        key: Key::Named(key),
        mods: 0,
        action: KeyAction::Press,
        text: String::new(),
        shifted: None,
        base_layout: None,
    }
}

#[test]
fn kitty_keyboard_off_keeps_keys_legacy() {
    let mut t = term(10, 3);
    t.set_config(EngineConfig {
        kitty_keyboard: false,
        ..EngineConfig::default()
    });
    t.advance(b"\x1b[>1u");
    assert!(!t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
    assert_eq!(t.key(&press(NamedKey::Escape)), b"\x1b");
    assert!(t.key(&press(NamedKey::F(30))).is_empty());
}

#[test]
fn f26_to_f35_use_kitty_codes() {
    let mut t = term(10, 3);
    // No legacy sequence exists for them.
    assert!(t.key(&press(NamedKey::F(26))).is_empty());
    t.advance(b"\x1b[>1u");
    assert_eq!(t.key(&press(NamedKey::F(26))), b"\x1b[57389u");
    assert_eq!(t.key(&press(NamedKey::F(35))), b"\x1b[57398u");
    let mut ev = press(NamedKey::F(30));
    ev.mods = mods::CTRL | mods::SHIFT;
    assert_eq!(t.key(&ev), b"\x1b[57393;6u");
    // Releases only when event types are reported.
    ev.action = KeyAction::Release;
    assert!(t.key(&ev).is_empty());
    t.advance(b"\x1b[=3u");
    assert_eq!(t.key(&ev), b"\x1b[57393;6:3u");
}

// ---- kitty graphics ------------------------------------------------------------------------

fn rgba_payload(w: u32, h: u32) -> String {
    use base64::Engine;
    let data = vec![255u8; (w * h * 4) as usize];
    base64::engine::general_purpose::STANDARD.encode(data)
}

#[test]
fn kitty_image_place_scroll_and_delete() {
    let mut t = term(20, 5);
    let mut view = ClientView::new();
    // 20x40 px image = 2x2 cells with 10x20 cells.
    let cmd = format!(
        "\x1b_Ga=T,f=32,s=20,v=40,i=7;{}\x1b\\",
        rgba_payload(20, 40)
    );
    t.advance(b"ab");
    t.advance(cmd.as_bytes());
    let ev = t.drain_events();
    assert!(
        ev.iter()
            .any(|e| matches!(e, TermEvent::PtyWrite(b) if b.windows(2).any(|w| w == b"OK"))),
        "{ev:?}"
    );
    let s = t.snapshot(1, &mut view).unwrap();
    assert_eq!(s.frame.images.len(), 1);
    let p = &s.frame.images[0];
    assert_eq!((p.image, p.row, p.col, p.cols, p.rows), (7, 0, 2, 2, 2));
    assert_eq!(s.new_images.len(), 1);
    assert_eq!(s.new_images[0].width, 20);
    // Cursor moved below/after the image.
    assert_eq!((s.frame.cursor.row, s.frame.cursor.col), (1, 4));

    // Scroll the image up by 1 line: it follows the text.
    t.advance(b"\r\n\r\n\r\n\r\n");
    let s = t.snapshot(1, &mut view).unwrap();
    assert_eq!(s.frame.images[0].row, -1);
    assert!(
        s.new_images.is_empty(),
        "image data is sent once per client"
    );

    // Delete by id.
    t.advance(b"\x1b_Ga=d,d=i,i=7\x1b\\");
    let s = t.snapshot(1, &mut view).unwrap();
    assert!(s.frame.images.is_empty());
}

#[test]
fn kitty_image_erased_with_screen() {
    let mut t = term(20, 5);
    let cmd = format!(
        "\x1b_Ga=T,f=32,s=10,v=20,i=3,q=2;{}\x1b\\",
        rgba_payload(10, 20)
    );
    t.advance(cmd.as_bytes());
    t.advance(b"\x1b[2J");
    let mut view = ClientView::new();
    let s = t.snapshot(1, &mut view).unwrap();
    assert!(s.frame.images.is_empty());
}

#[test]
fn kitty_anchor_links_are_hidden() {
    let mut t = term(20, 5);
    let cmd = format!(
        "\x1b_Ga=T,f=32,s=10,v=20,i=3,q=2;{}\x1b\\",
        rgba_payload(10, 20)
    );
    t.advance(cmd.as_bytes());
    let mut view = ClientView::new();
    let s = t.snapshot(1, &mut view).unwrap();
    assert!(s.frame.links.is_empty());
    assert!(
        s.frame
            .lines
            .iter()
            .all(|l| l.cells.iter().all(|c| c.link == 0))
    );
}

/// Client-side model of a pane: applies frames the way `thurm-ffi` does.
fn apply_rows(model: &mut Vec<String>, f: &Frame) {
    let rows = f.rows as usize;
    if f.full || model.len() != rows {
        *model = vec![String::new(); rows];
    }
    if f.shift != 0 && !f.full {
        let old = std::mem::take(model);
        *model = (0..rows as i64)
            .map(|r| {
                usize::try_from(r - f.shift as i64)
                    .ok()
                    .and_then(|s| old.get(s).cloned())
                    .unwrap_or_default()
            })
            .collect();
    }
    for l in &f.lines {
        model[l.row as usize] = row_text(f, l.row);
    }
}

#[test]
fn scrolling_shifts_rows_instead_of_resending() {
    let mut t = term(20, 5);
    for i in 0..30 {
        t.advance(format!("line {i}\r\n").as_bytes());
    }
    let mut view = ClientView::new();
    let mut model = Vec::new();
    apply_rows(&mut model, &t.snapshot(1, &mut view).unwrap().frame);

    t.scroll(ScrollCmd::Lines(2));
    let f = t.snapshot(1, &mut view).unwrap().frame;
    assert_eq!(f.shift, 2);
    assert_eq!(f.lines.len(), 2, "only the two exposed rows are sent");
    assert!(f.has_peek);
    let peek = f.peek.as_ref().expect("peek row");
    let peek_text: String = peek.cells.iter().map(|c| c.ch).collect();
    assert_eq!(peek_text.trim_end(), "line 23");
    apply_rows(&mut model, &f);

    // Mixed moves (and a jump larger than the screen) must match a fresh full frame.
    for cmd in [
        ScrollCmd::Lines(-1),
        ScrollCmd::Offset(20),
        ScrollCmd::Lines(3),
        ScrollCmd::Offset(4),
        ScrollCmd::Bottom,
    ] {
        t.scroll(cmd);
        if let Some(s) = t.snapshot(1, &mut view) {
            apply_rows(&mut model, &s.frame);
        }
        let mut fresh_model = Vec::new();
        apply_rows(
            &mut fresh_model,
            &t.snapshot(1, &mut ClientView::new()).unwrap().frame,
        );
        assert_eq!(model, fresh_model, "after {cmd:?}");
    }

    t.scroll(ScrollCmd::Offset(u32::MAX));
    let f = t.snapshot(1, &mut view).unwrap().frame;
    assert_eq!(f.display_offset, f.history_size);
    assert!(!f.has_peek, "nothing above the top of history");
}

fn responses(t: &mut Terminal) -> String {
    t.drain_events()
        .into_iter()
        .filter_map(|e| match e {
            TermEvent::PtyWrite(b) => Some(String::from_utf8_lossy(&b).into_owned()),
            _ => None,
        })
        .collect()
}

#[test]
fn kitty_unicode_placeholders() {
    let mut t = term(20, 5);
    let mut view = ClientView::new();
    // 40x40 px image in a virtual 4x2-cell placement (cells are 10x20): fitted to 40x40,
    // centered horizontally in the 40x40 box.
    let cmd = format!(
        "\x1b_Ga=T,U=1,f=32,s=40,v=40,i=42,c=4,r=2,q=2;{}\x1b\\",
        rgba_payload(40, 40)
    );
    t.advance(cmd.as_bytes());
    assert_eq!(responses(&mut t), "", "q=2 is silent");
    let s = t.snapshot(1, &mut view).unwrap();
    assert!(
        s.frame.images.is_empty(),
        "a virtual placement shows nothing by itself"
    );
    assert_eq!(
        (s.frame.cursor.row, s.frame.cursor.col),
        (0, 0),
        "and doesn't move the cursor"
    );

    // Two rows of placeholders: image 42 in the fg color, row diacritics, columns inherited.
    t.advance(
        "\x1b[38;5;42m\u{10EEEE}\u{0305}\u{10EEEE}\u{10EEEE}\u{10EEEE}\x1b[39m\r\n".as_bytes(),
    );
    t.advance("\x1b[38;5;42m\u{10EEEE}\u{030D}\u{10EEEE}\u{10EEEE}\u{10EEEE}\x1b[39m".as_bytes());
    let s = t.snapshot(1, &mut view).unwrap();
    assert_eq!(s.frame.images.len(), 2, "one slice per row");
    let top = &s.frame.images[0];
    assert_eq!((top.image, top.row, top.col), (42, 0, 0));
    assert_eq!((top.dst_w, top.dst_h), (40, 20));
    assert_eq!((top.src_y, top.src_h), (0, 20));
    let bottom = &s.frame.images[1];
    assert_eq!((bottom.row, bottom.src_y, bottom.src_h), (1, 20, 20));
    assert_eq!(s.new_images.len(), 1);
    // The placeholder characters themselves are not drawn.
    assert!(
        s.frame
            .lines
            .iter()
            .all(|l| l.cells.iter().all(|c| c.ch != '\u{10EEEE}'))
    );

    // "a" (delete all) must not remove virtual placements; "d=i" does.
    t.advance(b"\x1b_Ga=d,d=a\x1b\\");
    assert_eq!(
        t.snapshot(1, &mut ClientView::new())
            .unwrap()
            .frame
            .images
            .len(),
        2
    );
    t.advance(b"\x1b_Ga=d,d=i,i=42\x1b\\");
    assert!(
        t.snapshot(1, &mut ClientView::new())
            .unwrap()
            .frame
            .images
            .is_empty()
    );
}

#[test]
fn kitty_shared_memory_transmission() {
    use base64::Engine;
    let name = format!("/thurm-test-{}", std::process::id());
    let cname = std::ffi::CString::new(name.clone()).unwrap();
    let data = [200u8; 2 * 2 * 4];
    unsafe {
        let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
        assert!(fd >= 0);
        assert_eq!(libc::ftruncate(fd, data.len() as libc::off_t), 0);
        let map = libc::mmap(
            std::ptr::null_mut(),
            data.len(),
            libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        std::ptr::copy_nonoverlapping(data.as_ptr(), map as *mut u8, data.len());
        libc::munmap(map, data.len());
        libc::close(fd);
    }
    let mut t = term(20, 5);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&name);
    t.advance(
        format!(
            "\x1b_Ga=t,t=s,f=32,s=2,v=2,i=5,S={};{b64}\x1b\\",
            data.len()
        )
        .as_bytes(),
    );
    assert!(responses(&mut t).contains("OK"));
    assert_eq!(t.image(5).map(|i| (i.width, i.height)), Some((2, 2)));
    // The terminal unlinks the object after reading it.
    assert!(unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0) } < 0);
}

#[test]
fn input_line_after_prompt_mark() {
    let mut t = term(20, 5);
    assert_eq!(t.input_line(), None, "no prompt mark yet");
    t.advance(b"\x1b]133;A\x07~ \xe2\x9d\xaf \x1b]133;B\x07git ch");
    assert_eq!(t.input_line().as_deref(), Some("git ch"));
    // Input starting at column 0 (empty prompt).
    let mut t = term(20, 5);
    t.advance(b"\x1b]133;A\x07\x1b]133;B\x07ls -l");
    assert_eq!(t.input_line().as_deref(), Some("ls -l"));
    // Wrapped input keeps going on the next line.
    let mut t = term(10, 5);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07echo 12345678");
    assert_eq!(t.input_line().as_deref(), Some("echo 12345678"));
}

fn all_text(t: &Terminal) -> String {
    t.capture(&CaptureOpts {
        lines: None,
        scrollback: true,
        ansi: false,
    })
}

#[test]
fn attach_state_round_trips() {
    let mut a = term(20, 5);
    for i in 0..12 {
        a.advance(format!("\x1b[3{}mline {i}\x1b[0m\r\n", i % 7).as_bytes());
    }
    a.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi");
    let mut b = term(20, 5);
    b.replay(&a.serialize_state());
    assert_eq!(all_text(&b), all_text(&a));
    assert_eq!(
        b.input_line().as_deref(),
        Some("echo hi"),
        "prompt marks survive"
    );
    let (ca, cb) = (
        a.snapshot(1, &mut ClientView::new()).unwrap(),
        b.snapshot(1, &mut ClientView::new()).unwrap(),
    );
    assert_eq!(
        (cb.frame.cursor.row, cb.frame.cursor.col),
        (ca.frame.cursor.row, ca.frame.cursor.col)
    );
    assert_eq!(
        cb.frame.lines[0].cells[0].fg, ca.frame.lines[0].cells[0].fg,
        "colors survive"
    );

    // A full-screen app: alternate screen, mouse + bracketed paste, kitty keys, scroll region.
    a.advance(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[>1u\x1b[2;4r\x1b[1;1HTUI top\x1b[3;2Hmiddle");
    let alt_before = a.screen_text();
    let state = a.serialize_state();
    assert_eq!(
        a.screen_text(),
        alt_before,
        "the daemon's own alternate screen is intact"
    );
    assert!(a.is_alt_screen());
    let mut c = term(20, 5);
    c.replay(&state);
    assert!(c.is_alt_screen());
    assert_eq!(c.screen_text(), alt_before);
    let bits = TermMode::MOUSE_REPORT_CLICK
        | TermMode::SGR_MOUSE
        | TermMode::BRACKETED_PASTE
        | TermMode::DISAMBIGUATE_ESC_CODES
        | TermMode::ALT_SCREEN;
    assert_eq!(c.mode() & bits, a.mode() & bits);
    // Scroll region: a line feed at its bottom scrolls only rows 2..4 in both copies.
    for t in [&mut a, &mut c] {
        t.advance(b"\x1b[4;1Hbottom\n");
    }
    assert_eq!(c.screen_text(), a.screen_text());
    // Leaving the alternate screen brings back the primary screen in the copy too.
    for t in [&mut a, &mut c] {
        t.advance(b"\x1b[?1049l");
    }
    assert_eq!(all_text(&c), all_text(&a));
}

#[test]
fn forwarded_stream_keeps_copies_in_sync() {
    // The copy sees only the forwarded stream, including a temp-file image the daemon consumed.
    let dir = std::env::temp_dir().join(format!(
        "tty-graphics-protocol-thurm-{}",
        std::process::id()
    ));
    std::fs::write(&dir, vec![9u8; 2 * 2 * 4]).unwrap();
    use base64::Engine;
    let path = base64::engine::general_purpose::STANDARD.encode(dir.to_str().unwrap());
    let mut a = term(20, 5);
    let mut b = term(20, 5);
    let input = format!(
        "hello\x1b]7;file://host/tmp\x07\x1b]133;A\x07$ \x1b]133;B\x07\x1b_Ga=T,t=t,f=32,s=2,v=2,i=3;{path}\x1b\\done"
    );
    // Split mid-escape to exercise chunk boundaries.
    let bytes = input.as_bytes();
    let (x, y) = bytes.split_at(9);
    for part in [x, y] {
        let fwd = a.advance_forward(part);
        b.advance(&fwd);
    }
    assert!(!dir.exists(), "the daemon's copy consumed the temp file");
    assert_eq!(b.screen_text(), a.screen_text());
    assert_eq!(
        b.image(3).map(|i| (i.width, i.height)),
        Some((2, 2)),
        "image arrived as a direct transmission"
    );
    let pa = a.snapshot(1, &mut ClientView::new()).unwrap().frame.images;
    let pb = b.snapshot(1, &mut ClientView::new()).unwrap().frame.images;
    assert_eq!(pa.len(), 1);
    assert_eq!(pb.len(), 1);
    assert_eq!((pb[0].row, pb[0].col), (pa[0].row, pa[0].col));
}

#[test]
fn resize_at_a_redrawing_prompt_clears_it_in_every_copy() {
    let resize = |t: &mut Terminal, cols: u16| {
        let mut s = t.size();
        s.cols = cols;
        t.resize(s);
    };
    // A prompt with a padded right prompt, as fish draws it; the shell redraws on resize.
    let prompt = "\x1b]133;A;click_events=1\x07~/dev ❯ \x1b]133;B\x07cd x          RIGHT";
    let mut a = term(40, 5);
    a.advance(b"old output\r\n");
    a.advance(prompt.as_bytes());
    // A copy attached before the resize must clear the same way.
    let mut b = term(40, 5);
    b.replay(&a.serialize_state());
    for t in [&mut a, &mut b] {
        resize(t, 20);
    }
    assert_eq!(
        a.screen_text().trim_end(),
        "old output",
        "prompt cleared, output kept"
    );
    assert_eq!(all_text(&b), all_text(&a));
    // No frame shows the erased prompt: they wait for the shell's redraw, which here doesn't
    // come, so until the hold runs out.
    for t in [&mut a, &mut b] {
        assert!(
            t.snapshot(1, &mut ClientView::new()).is_none(),
            "held for the redraw"
        );
        let deadline = t.sync_deadline().expect("a deadline to flush at");
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        t.flush_sync();
    }
    let (ca, cb) = (
        a.snapshot(1, &mut ClientView::new()).unwrap(),
        b.snapshot(1, &mut ClientView::new()).unwrap(),
    );
    // The cursor stays where it was (libghostty-vt, like Ghostty: the shell redraws from
    // there), in every copy.
    assert_eq!(
        (cb.frame.cursor.row, cb.frame.cursor.col),
        (ca.frame.cursor.row, ca.frame.cursor.col)
    );

    // A running command (133;C) and shells that don't redraw keep what is on screen.
    for marks in [
        "\x1b]133;A;redraw=1\x07$ \x1b]133;C\x07running",
        "\x1b]133;A;redraw=0\x07$ waiting",
    ] {
        let mut t = term(40, 5);
        t.advance(marks.as_bytes());
        let before = t.screen_text();
        resize(&mut t, 30);
        assert_eq!(t.screen_text().trim_end(), before.trim_end(), "{marks:?}");
        let mut copy = term(30, 5);
        copy.replay(&t.serialize_state());
        resize(&mut copy, 25);
        assert_eq!(
            copy.screen_text().trim_end(),
            before.trim_end(),
            "{marks:?}"
        );
    }
}

#[test]
fn clear_screen_keeps_the_prompt_at_the_top() {
    let mut t = term(20, 5);
    for i in 0..12 {
        t.advance(format!("line {i}\r\n").as_bytes());
    }
    t.advance(b"\x1b]133;A\x07~ \xe2\x9d\xaf \x1b]133;B\x07git ch");
    t.clear_screen();
    let screen = t.screen_text();
    let mut lines = screen.lines();
    assert_eq!(lines.next().map(str::trim_end), Some("~ \u{276f} git ch"));
    assert!(lines.all(|l| l.trim().is_empty()), "{screen:?}");
    assert!(!all_text(&t).contains("line"), "scrollback cleared");
    // What's typed is still recognized, now on the top row.
    assert_eq!(t.input_line().as_deref(), Some("git ch"));

    // Not at a prompt: the cursor's line moves up.
    let mut t = term(20, 5);
    t.advance(b"a\r\nb\r\nc\r\nprogress 50%");
    t.clear_screen();
    assert_eq!(
        t.screen_text().lines().next().map(str::trim_end),
        Some("progress 50%")
    );

    // A full-screen program keeps its screen.
    let mut t = term(20, 5);
    t.advance(b"old\r\n\x1b[?1049h\x1b[3;1Hvim");
    t.clear_screen();
    assert_eq!(
        t.screen_text().lines().nth(2).map(str::trim_end),
        Some("vim")
    );
}

#[test]
fn prompt_redraw_after_resize_shows_as_one_frame() {
    let mut t = term(40, 5);
    t.advance("\x1b]133;A;redraw=1\x07~/dev \u{276f} \x1b]133;B\x07ls".as_bytes());
    let mut view = ClientView::new();
    t.snapshot(1, &mut view).unwrap();
    let mut s = t.size();
    s.cols = 30;
    t.resize(s);
    let started = Instant::now();
    assert!(
        t.snapshot(1, &mut view).is_none(),
        "the erased prompt is never shown"
    );
    // Fish sends settings and a title before it draws the prompt.
    let cap = t.sync_deadline().unwrap();
    t.advance(b"\x1b[?2004l\x1b[?2031l\x1b[=0u");
    t.advance(b"\x1b]0;title\x07\x1b[m\x1b[?2004h\r");
    assert_eq!(
        t.sync_deadline(),
        Some(cap),
        "settings are not a prompt redraw"
    );
    assert!(t.snapshot(1, &mut view).is_none());
    // The shell redraws: shown as soon as its output settles, well before the cap.
    t.advance("\r\x1b]133;A\x07~/dev \u{276f} \x1b]133;B\x07ls".as_bytes());
    let deadline = t.sync_deadline().unwrap();
    assert!(
        deadline - started < Duration::from_millis(50),
        "settles, not the 100 ms cap"
    );
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    t.flush_sync();
    let f = t.snapshot(1, &mut view).unwrap();
    assert_eq!(t.sync_deadline(), None);
    assert!(f.frame.full);
    assert_eq!(
        t.screen_text().lines().next().map(str::trim_end),
        Some("~/dev \u{276f} ls")
    );
}

#[test]
fn device_queries_are_answered() {
    let mut t = term(20, 5);
    t.advance(b"ab\x1b[c");
    assert_eq!(responses(&mut t), "\x1b[?6c");
    t.advance(b"\x1b[>c");
    assert!(responses(&mut t).starts_with("\x1b[>0;"));
    t.advance(b"\x1b[6n");
    assert_eq!(responses(&mut t), "\x1b[1;3R");
    t.advance(b"\x1b[18t");
    assert_eq!(responses(&mut t), "\x1b[8;5;20t");
    t.advance(b"\x1b[14t");
    assert_eq!(responses(&mut t), "\x1b[4;100;200t");
    t.advance(b"\x1b[>q");
    assert_eq!(responses(&mut t), "\x1bP>|Thurm 0.1.0\x1b\\");
}

#[test]
fn scrollback_keeps_the_configured_lines() {
    for cols in [80, 600] {
        let lines = 1_000;
        let size = PaneSize {
            cols,
            rows: 24,
            cell_width: 10,
            cell_height: 20,
        };
        let cfg = EngineConfig {
            scrollback: lines,
            ..EngineConfig::default()
        };
        let mut t = Terminal::new(size, cfg);
        let out: String = (0..lines + 2_000)
            .map(|i| format!("line {i}\r\n"))
            .collect();
        t.advance(out.as_bytes());
        assert!(
            t.vt.history() >= lines,
            "{cols} cols: {} of {lines} lines kept",
            t.vt.history()
        );
    }
}

#[test]
fn kitty_png_images_are_decoded() {
    use base64::Engine;
    let mut png = Vec::new();
    {
        let mut enc = ::png::Encoder::new(&mut png, 3, 2);
        enc.set_color(::png::ColorType::Rgb);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[9u8; 3 * 2 * 3]).unwrap();
    }
    let mut t = term(20, 5);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    t.advance(format!("\x1b_Ga=T,f=100,i=4;{b64}\x1b\\").as_bytes());
    assert!(responses(&mut t).contains("OK"));
    let img = t.image(4).expect("stored");
    assert_eq!((img.width, img.height), (3, 2));
    assert_eq!(&img.rgba[..4], &[9, 9, 9, 255]);
}

#[test]
fn kitty_file_transmission_reaches_every_copy() {
    use base64::Engine;
    let dir = std::env::temp_dir().join(format!(
        "tty-graphics-protocol-thurm-dir-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("img");
    std::fs::write(&path, [50u8; 2 * 2 * 4]).unwrap();
    let name = base64::engine::general_purpose::STANDARD.encode(path.to_str().unwrap());
    let mut daemon = term(20, 5);
    let forwarded =
        daemon.advance_forward(format!("\x1b_Ga=T,t=t,f=32,s=2,v=2,i=6;{name}\x1b\\").as_bytes());
    assert!(responses(&mut daemon).contains("OK"));
    assert!(!path.exists(), "temporary files are deleted after reading");
    // The app's copy gets the image from the forwarded stream alone.
    let mut app = term(20, 5);
    app.advance(&forwarded);
    for t in [&mut daemon, &mut app] {
        let s = t.snapshot(1, &mut ClientView::new()).unwrap();
        assert_eq!(s.frame.images.len(), 1);
        assert_eq!(s.new_images[0].rgba.len(), 16);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn kitty_deleted_images_are_reported() {
    let mut t = term(20, 5);
    let mut view = ClientView::new();
    t.advance(
        format!(
            "\x1b_Ga=T,f=32,s=10,v=20,i=8,q=2;{}\x1b\\",
            rgba_payload(10, 20)
        )
        .as_bytes(),
    );
    let s = t.snapshot(1, &mut view).unwrap();
    assert_eq!(s.new_images.len(), 1);
    t.drain_events();
    t.advance(b"\x1b_Ga=d,d=I,i=8\x1b\\");
    assert!(
        t.drain_events()
            .iter()
            .any(|e| matches!(e, TermEvent::ImageFreed(8)))
    );
    // Sent again after a retransmission with the same id.
    t.advance(
        format!(
            "\x1b_Ga=T,f=32,s=10,v=20,i=8,q=2;{}\x1b\\",
            rgba_payload(10, 20)
        )
        .as_bytes(),
    );
    assert_eq!(t.snapshot(1, &mut view).unwrap().new_images.len(), 1);
}

#[test]
fn progress_reports_keep_their_state() {
    use thurm_proto::{Progress, ProgressState as S};
    let mut t = term(20, 5);
    t.advance(b"\x1b]9;4;3\x07\x1b]9;4;1;42\x1b\\\x1b]9;4;2\x07\x1b]9;4;0\x07");
    let progress: Vec<_> = t
        .drain_events()
        .into_iter()
        .filter_map(|e| match e {
            TermEvent::Progress(p) => Some(p),
            _ => None,
        })
        .collect();
    let p = |state, percent| Some(Progress { state, percent });
    assert_eq!(
        progress,
        [
            p(S::Indeterminate, None),
            p(S::Normal, Some(42)),
            p(S::Error, None),
            None
        ]
    );
}

#[test]
fn cwd_and_clipboard_come_from_libghostty() {
    let mut t = term(20, 5);
    t.advance(b"\x1b]9;9;/Users/me/src\x1b\\\x1b]1337;CurrentDir=/tmp\x07");
    let ev = t.drain_events();
    assert!(
        ev.contains(&TermEvent::Cwd("/Users/me/src".into())),
        "{ev:?}"
    );
    assert!(ev.contains(&TermEvent::Cwd("/tmp".into())), "{ev:?}");
    // OSC 9;9 is not a notification.
    assert!(
        !ev.iter().any(|e| matches!(e, TermEvent::Notify { .. })),
        "{ev:?}"
    );
    // OSC 52 writes (base64 "hi") and iTerm2 copies.
    t.advance(b"\x1b]52;c;aGk=\x07");
    assert!(
        t.drain_events()
            .contains(&TermEvent::ClipboardStore("hi".into()))
    );
}

#[test]
fn prompt_mark_columns_use_the_terminals_widths() {
    assert_eq!(byte_offset_of_column("日x", 2, true), "日".len());
    assert_eq!(
        byte_offset_of_column("\x1b[1m日\x1b[0mx", 2, true),
        "\x1b[1m日\x1b[0m".len()
    );
    assert_eq!(
        byte_offset_of_column("e\u{301}x", 1, true),
        "e\u{301}".len()
    );
    // An emoji presentation selector makes a wide cluster with grapheme clustering (2027),
    // and changes nothing without it.
    let heart = "\u{2764}\u{fe0f}x";
    assert_eq!(byte_offset_of_column(heart, 2, true), heart.len() - 1);
    assert_eq!(byte_offset_of_column(heart, 1, false), heart.len() - 1);
    assert_eq!(byte_offset_of_column("ab", 5, true), 2);
}

#[test]
fn prompt_marks_are_libghosttys() {
    // VS Code's marks work like OSC 133 ones.
    let mut t = term(20, 5);
    t.advance(b"\x1b]633;A\x07$ \x1b]633;B\x07hi");
    assert_eq!(t.input_line().as_deref(), Some("hi"));
    assert_eq!(t.prompt_lines(), vec![0]);

    // History with three prompts, replayed into a copy: prompt jumping works there, and only
    // the current prompt is an event.
    let mut a = term(20, 10);
    for cmd in ["ls", "pwd"] {
        a.advance(
            format!("\x1b]133;A\x07$ \x1b]133;B\x07{cmd}\r\n\x1b]133;C\x07out\r\n").as_bytes(),
        );
    }
    a.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ec");
    let mut b = term(20, 10);
    b.replay(&a.serialize_state());
    assert_eq!(b.prompt_lines(), a.prompt_lines());
    assert_eq!(a.prompt_lines().len(), 3);
    let prompts = b
        .drain_events()
        .iter()
        .filter(|e| matches!(e, TermEvent::PromptStart))
        .count();
    assert_eq!(prompts, 1);
    assert_eq!(b.input_line().as_deref(), Some("ec"));
}

mod encoding {
    //! Key, mouse, wheel and focus encoding through libghostty-vt's encoders, with modes set
    //! by the program's escape sequences.
    use super::*;
    use thurm_proto::{Key, MouseButton, MouseKind, NamedKey};

    fn key(k: Key, m: u8, text: &str) -> KeyEvent {
        KeyEvent {
            key: k,
            mods: m,
            action: KeyAction::Press,
            text: text.into(),
            shifted: None,
            base_layout: None,
        }
    }
    fn ch(c: char, m: u8, text: &str) -> KeyEvent {
        key(Key::Char(c), m, text)
    }
    fn named(k: NamedKey, m: u8) -> KeyEvent {
        key(Key::Named(k), m, "")
    }
    /// A terminal after `setup` (mode escapes).
    fn with(setup: &str) -> Terminal {
        let mut t = term(80, 24);
        t.advance(setup.as_bytes());
        t.drain_events();
        t
    }
    fn enc(setup: &str, ev: &KeyEvent) -> Vec<u8> {
        with(setup).key(ev)
    }
    fn legacy(ev: &KeyEvent) -> Vec<u8> {
        enc("", ev)
    }
    fn kitty(ev: &KeyEvent, flags: u8) -> String {
        String::from_utf8(enc(&format!("\x1b[>{flags}u"), ev)).unwrap()
    }
    const D: u8 = 1;
    const EVENTS: u8 = 2;
    const ALTERNATES: u8 = 4;
    const ALL: u8 = 8;
    const TEXT: u8 = 16;

    #[test]
    fn legacy_text_and_ctrl() {
        assert_eq!(legacy(&ch('a', 0, "a")), b"a");
        assert_eq!(legacy(&ch('a', mods::SHIFT, "A")), b"A");
        assert_eq!(legacy(&ch('c', mods::CTRL, "")), b"\x03");
        assert_eq!(legacy(&ch('c', mods::CTRL | mods::SHIFT, "")), b"\x03");
        assert_eq!(legacy(&ch(' ', mods::CTRL, "")), b"\x00");
        // Ctrl+[ / Ctrl+i / Ctrl+m are CSI u (fixterms), as in Ghostty.
        assert_eq!(legacy(&ch('[', mods::CTRL, "")), b"\x1b[91;5u");
        assert_eq!(legacy(&ch('/', mods::CTRL, "")), b"\x1f");
        assert_eq!(legacy(&ch('b', mods::ALT, "")), b"\x1bb");
        assert_eq!(legacy(&ch('b', mods::ALT, "b")), b"\x1bb");
        assert_eq!(legacy(&ch('x', mods::CTRL | mods::ALT, "")), b"\x1b\x18");
        assert_eq!(legacy(&ch('ł', 0, "ł")), "ł".as_bytes());
        assert_eq!(legacy(&ch('a', mods::CAPS_LOCK, "A")), b"A");
    }

    #[test]
    fn legacy_named() {
        assert_eq!(legacy(&named(NamedKey::Enter, 0)), b"\r");
        assert_eq!(legacy(&named(NamedKey::Enter, mods::ALT)), b"\x1b\r");
        assert_eq!(legacy(&named(NamedKey::Tab, 0)), b"\t");
        assert_eq!(legacy(&named(NamedKey::Tab, mods::SHIFT)), b"\x1b[Z");
        assert_eq!(legacy(&named(NamedKey::Backspace, 0)), b"\x7f");
        assert_eq!(legacy(&named(NamedKey::Backspace, mods::CTRL)), b"\x08");
        assert_eq!(legacy(&named(NamedKey::Backspace, mods::ALT)), b"\x1b\x7f");
        assert_eq!(legacy(&named(NamedKey::Escape, 0)), b"\x1b");
        assert_eq!(legacy(&named(NamedKey::Up, 0)), b"\x1b[A");
        assert_eq!(enc("\x1b[?1h", &named(NamedKey::Up, 0)), b"\x1bOA");
        assert_eq!(legacy(&named(NamedKey::Left, mods::CTRL)), b"\x1b[1;5D");
        assert_eq!(
            legacy(&named(NamedKey::Right, mods::SHIFT | mods::ALT)),
            b"\x1b[1;4C"
        );
        assert_eq!(legacy(&named(NamedKey::Delete, 0)), b"\x1b[3~");
        assert_eq!(legacy(&named(NamedKey::PageUp, mods::SHIFT)), b"\x1b[5;2~");
        assert_eq!(legacy(&named(NamedKey::F(1), 0)), b"\x1bOP");
        assert_eq!(legacy(&named(NamedKey::F(4), mods::CTRL)), b"\x1b[1;5S");
        assert_eq!(legacy(&named(NamedKey::F(5), 0)), b"\x1b[15~");
        assert_eq!(legacy(&named(NamedKey::F(12), mods::SHIFT)), b"\x1b[24;2~");
        assert_eq!(legacy(&named(NamedKey::F(20), 0)), b"\x1b[34~");
        assert_eq!(legacy(&named(NamedKey::LeftShift, 0)), b"");
        assert_eq!(legacy(&named(NamedKey::CapsLock, 0)), b"");
        assert_eq!(enc("\x1b[20h", &named(NamedKey::Enter, 0)), b"\r\n");
    }

    #[test]
    fn legacy_keypad_and_release() {
        assert_eq!(legacy(&key(Key::Named(NamedKey::Kp5), 0, "5")), b"5");
        // Application keypad applies once the program turns off "ignore it with num lock"
        // (mode 1035, on by default as in xterm).
        let app = "\x1b=\x1b[?1035l";
        assert_eq!(enc("\x1b=", &key(Key::Named(NamedKey::Kp5), 0, "5")), b"5");
        assert_eq!(enc(app, &key(Key::Named(NamedKey::Kp5), 0, "5")), b"\x1bOu");
        assert_eq!(
            enc(app, &key(Key::Named(NamedKey::KpEnter), 0, "\r")),
            b"\x1bOM"
        );
        assert_eq!(legacy(&named(NamedKey::KpEnter, 0)), b"\r");
        let mut r = ch('a', 0, "a");
        r.action = KeyAction::Release;
        assert!(legacy(&r).is_empty());
        let mut rep = ch('a', 0, "a");
        rep.action = KeyAction::Repeat;
        assert_eq!(legacy(&rep), b"a");
    }

    #[test]
    fn kitty_disambiguate() {
        assert_eq!(kitty(&ch('a', 0, "a"), D), "a");
        assert_eq!(kitty(&ch('a', mods::SHIFT, "A"), D), "A");
        assert_eq!(kitty(&named(NamedKey::Escape, 0), D), "\x1b[27u");
        assert_eq!(kitty(&ch('a', mods::CTRL, ""), D), "\x1b[97;5u");
        assert_eq!(kitty(&ch('a', mods::ALT, "a"), D), "\x1b[97;3u");
        assert_eq!(
            kitty(&ch('a', mods::CTRL | mods::SHIFT, ""), D),
            "\x1b[97;6u"
        );
        assert_eq!(kitty(&named(NamedKey::Enter, 0), D), "\r");
        assert_eq!(kitty(&named(NamedKey::Tab, 0), D), "\t");
        assert_eq!(kitty(&named(NamedKey::Tab, mods::SHIFT), D), "\x1b[9;2u");
        assert_eq!(kitty(&named(NamedKey::Backspace, 0), D), "\x7f");
        assert_eq!(kitty(&named(NamedKey::Enter, mods::CTRL), D), "\x1b[13;5u");
        assert_eq!(
            kitty(&named(NamedKey::Backspace, mods::ALT), D),
            "\x1b[127;3u"
        );
        assert_eq!(kitty(&named(NamedKey::Up, 0), D), "\x1b[A");
        assert_eq!(kitty(&named(NamedKey::Up, mods::SHIFT), D), "\x1b[1;2A");
        assert_eq!(kitty(&named(NamedKey::Delete, 0), D), "\x1b[3~");
        assert_eq!(kitty(&named(NamedKey::F(1), 0), D), "\x1b[P");
        assert_eq!(kitty(&named(NamedKey::F(1), mods::CTRL), D), "\x1b[1;5P");
        assert_eq!(kitty(&named(NamedKey::F(3), 0), D), "\x1b[13~");
        assert_eq!(kitty(&named(NamedKey::F(13), 0), D), "\x1b[57376u");
        assert_eq!(kitty(&named(NamedKey::Kp1, 0), D), "\x1b[57400u");
        assert_eq!(kitty(&named(NamedKey::LeftShift, mods::SHIFT), D), "");
        assert_eq!(kitty(&ch('a', mods::CAPS_LOCK, "A"), D), "A");
        assert_eq!(
            kitty(&named(NamedKey::Escape, mods::NUM_LOCK), D),
            "\x1b[27;129u"
        );
    }

    #[test]
    fn kitty_event_types() {
        let f = D | EVENTS;
        let mut ev = ch('a', 0, "a");
        assert_eq!(kitty(&ev, f), "a");
        ev.action = KeyAction::Repeat;
        assert_eq!(kitty(&ev, f), "a");
        ev.action = KeyAction::Release;
        assert_eq!(kitty(&ev, f), "\x1b[97;1:3u");
        let mut esc = named(NamedKey::Escape, 0);
        esc.action = KeyAction::Repeat;
        assert_eq!(kitty(&esc, f), "\x1b[27;1:2u");
        let mut up = named(NamedKey::Up, mods::CTRL);
        up.action = KeyAction::Release;
        assert_eq!(kitty(&up, f), "\x1b[1;5:3A");
        let mut enter = named(NamedKey::Enter, 0);
        enter.action = KeyAction::Release;
        assert_eq!(kitty(&enter, f), "");
        assert_eq!(kitty(&ev, D), "");
    }

    #[test]
    fn kitty_report_all() {
        let f = D | ALL;
        assert_eq!(kitty(&ch('a', 0, "a"), f), "\x1b[97u");
        assert_eq!(kitty(&ch('a', mods::SHIFT, "A"), f), "\x1b[97;2u");
        assert_eq!(kitty(&named(NamedKey::Enter, 0), f), "\x1b[13u");
        assert_eq!(kitty(&named(NamedKey::Tab, 0), f), "\x1b[9u");
        assert_eq!(kitty(&named(NamedKey::Backspace, 0), f), "\x1b[127u");
        assert_eq!(
            kitty(&named(NamedKey::LeftShift, mods::SHIFT), f),
            "\x1b[57441;2u"
        );
        assert_eq!(kitty(&named(NamedKey::CapsLock, 0), f), "\x1b[57358u");
        assert_eq!(kitty(&ch('a', mods::CAPS_LOCK, "A"), f), "\x1b[97;65u");
        let mut rel = named(NamedKey::Enter, 0);
        rel.action = KeyAction::Release;
        assert_eq!(kitty(&rel, f | EVENTS), "\x1b[13;1:3u");
    }

    #[test]
    fn kitty_alternates_and_text() {
        let f = D | ALL | ALTERNATES;
        let ev = ch('a', mods::SHIFT, "A");
        assert_eq!(kitty(&ev, f), "\x1b[97:65;2u");
        let ev2 = ch('a', 0, "a");
        assert_eq!(kitty(&ev2, f), "\x1b[97u");
        // Base layout key (Cyrillic с on the C key).
        let mut cy = ch('с', mods::CTRL, "");
        cy.base_layout = Some('c');
        assert_eq!(kitty(&cy, f), "\x1b[1089::99;5u");
        let both = ch('1', mods::SHIFT, "!");
        assert_eq!(kitty(&both, f), "\x1b[49:33;2u");

        let t = D | ALL | TEXT;
        assert_eq!(kitty(&ch('a', 0, "a"), t), "\x1b[97;;97u");
        assert_eq!(kitty(&ch('a', mods::SHIFT, "A"), t), "\x1b[97;2;65u");
        assert_eq!(kitty(&ch('c', mods::CTRL, "\x03"), t), "\x1b[99;5u");
        assert_eq!(kitty(&ch('a', 0, "a"), D | TEXT), "a");
        let mut rel = ch('a', 0, "a");
        rel.action = KeyAction::Release;
        assert_eq!(kitty(&rel, t | EVENTS), "\x1b[97;1:3u");
    }

    #[test]
    fn kitty_app_cursor_unmodified() {
        assert_eq!(
            enc("\x1b[>1u\x1b[?1h", &named(NamedKey::Down, 0)),
            b"\x1b[B"
        );
        assert_eq!(
            enc("\x1b[>1u\x1b[?1h", &named(NamedKey::Down, mods::ALT)),
            b"\x1b[1;3B"
        );
    }

    fn mouse(kind: MouseKind, button: MouseButton, col: u16, row: u16, m: u8) -> MouseEvent {
        MouseEvent {
            kind,
            button,
            col,
            row,
            right_half: false,
            mods: m,
            clicks: 1,
            x: 0,
            y: 0,
        }
    }
    fn report(setup: &str, ev: &MouseEvent) -> Option<Vec<u8>> {
        let mut t = with(setup);
        match t.mouse(ev) {
            (MouseOutcome::Reported, b) => Some(b),
            _ => None,
        }
    }

    #[test]
    fn mouse_sgr() {
        let mode = "\x1b[?1000h\x1b[?1006h";
        let p = mouse(MouseKind::Press, MouseButton::Left, 4, 9, 0);
        assert_eq!(report(mode, &p).unwrap(), b"\x1b[<0;5;10M");
        let r = mouse(MouseKind::Release, MouseButton::Right, 4, 9, mods::CTRL);
        assert_eq!(report(mode, &r).unwrap(), b"\x1b[<18;5;10m");
        let back = mouse(MouseKind::Press, MouseButton::Back, 0, 0, 0);
        assert_eq!(report(mode, &back).unwrap(), b"\x1b[<128;1;1M");
        assert!(report(mode, &mouse(MouseKind::Move, MouseButton::Left, 1, 1, 0)).is_none());
        let drag = "\x1b[?1002h\x1b[?1006h";
        assert_eq!(
            report(drag, &mouse(MouseKind::Move, MouseButton::Left, 1, 1, 0)).unwrap(),
            b"\x1b[<32;2;2M"
        );
        assert!(report(drag, &mouse(MouseKind::Move, MouseButton::None, 1, 1, 0)).is_none());
        let any = "\x1b[?1003h\x1b[?1006h";
        assert_eq!(
            report(
                any,
                &mouse(
                    MouseKind::Move,
                    MouseButton::None,
                    1,
                    1,
                    mods::CTRL | mods::ALT
                )
            )
            .unwrap(),
            b"\x1b[<59;2;2M"
        );
        // SGR pixel coordinates (1016), new with libghostty-vt.
        let mut px = mouse(MouseKind::Press, MouseButton::Left, 1, 0, 0);
        px.x = 14;
        px.y = 5;
        assert_eq!(
            report("\x1b[?1000h\x1b[?1016h", &px).unwrap(),
            b"\x1b[<0;14;5M"
        );
    }

    #[test]
    fn mouse_legacy_and_utf8() {
        let mode = "\x1b[?1000h";
        assert_eq!(
            report(mode, &mouse(MouseKind::Press, MouseButton::Left, 0, 0, 0)).unwrap(),
            b"\x1b[M !!"
        );
        assert_eq!(
            report(mode, &mouse(MouseKind::Release, MouseButton::Left, 0, 0, 0)).unwrap(),
            b"\x1b[M#!!"
        );
        assert_eq!(
            report(
                mode,
                &mouse(MouseKind::Release, MouseButton::Middle, 0, 0, mods::CTRL)
            )
            .unwrap(),
            b"\x1b[M3!!"
        );
        let mut wide = with(mode);
        let mut size = wide.size();
        size.cols = 400;
        wide.resize(size);
        let far = mouse(MouseKind::Press, MouseButton::Left, 300, 0, 0);
        assert!(
            !matches!(wide.mouse(&far).0, MouseOutcome::Reported) || wide.mouse(&far).1.is_empty()
        );
        wide.advance(b"\x1b[?1005h");
        let (_, b) = wide.mouse(&far);
        assert_eq!(b, "\x1b[M \u{14d}!".as_bytes());
        assert!(report("", &mouse(MouseKind::Press, MouseButton::Left, 0, 0, 0)).is_none());
    }

    #[test]
    fn wheel_and_focus() {
        let sgr = "\x1b[?1000h\x1b[?1006h";
        assert_eq!(with(sgr).wheel(2, 0, 0, 0), b"\x1b[<64;1;1M\x1b[<64;1;1M");
        assert_eq!(with(sgr).wheel(-1, 2, 3, mods::CTRL), b"\x1b[<81;3;4M");
        let alt = "\x1b[?1049h\x1b[?1007h";
        assert_eq!(with(alt).wheel(2, 0, 0, 0), b"\x1b[A\x1b[A");
        assert_eq!(
            with(&format!("{alt}\x1b[?1h")).wheel(-1, 0, 0, 0),
            b"\x1bOB"
        );
        assert!(with("").wheel(3, 0, 0, 0).is_empty());
        // Alternate scroll is on by default (libghostty-vt, like Ghostty); off when the
        // program turns it off.
        assert_eq!(with("\x1b[?1049h").wheel(1, 0, 0, 0), b"\x1b[A");
        assert!(with("\x1b[?1049h\x1b[?1007l").wheel(3, 0, 0, 0).is_empty());
        let mut f = with("\x1b[?1004h");
        assert_eq!(f.focus(true).unwrap(), b"\x1b[I");
        assert_eq!(f.focus(false).unwrap(), b"\x1b[O");
        assert!(with("").focus(true).is_none());
    }

    #[test]
    fn paste() {
        assert_eq!(with("").paste("a\nb"), b"a\rb");
        assert_eq!(
            with("\x1b[?2004h").paste("a\x1bb"),
            b"\x1b[200~a b\x1b[201~"
        );
        assert!(keys::paste_is_safe("ls"));
        assert!(!keys::paste_is_safe("rm -rf x\n"));
    }
}

#[test]
fn selections_are_libghosttys() {
    let mut t = term(20, 5);
    t.advance(b"abcd\r\nefgh\r\nijkl");
    // A rectangle: columns 1..=2 of the first two lines.
    t.selection(SelectionOp::Start {
        col: 1,
        row: 0,
        right_half: false,
        kind: SelectionKind::Block,
    });
    t.selection(SelectionOp::Update {
        col: 2,
        row: 1,
        right_half: true,
    });
    assert_eq!(t.selection_text(), "bc\nfg");

    // Shift-click moves the end of the selection.
    let click = |col, row, mods, kind| MouseEvent {
        kind,
        button: MouseButton::Left,
        col,
        row,
        right_half: true,
        mods,
        clicks: 1,
        x: 0,
        y: 0,
    };
    t.selection(SelectionOp::Start {
        col: 0,
        row: 0,
        right_half: false,
        kind: SelectionKind::Word,
    });
    assert_eq!(t.selection_text(), "abcd");
    t.mouse(&click(1, 2, mods::SHIFT, MouseKind::Press));
    t.mouse(&click(1, 2, mods::SHIFT, MouseKind::Release));
    assert_eq!(t.selection_text(), "abcd\nefgh\nij");

    // It stays on its text as output scrolls.
    t.selection(SelectionOp::Start {
        col: 0,
        row: 1,
        right_half: false,
        kind: SelectionKind::Line,
    });
    t.advance(b"\r\nm\r\nn\r\no");
    assert_eq!(t.selection_text(), "efgh");
}

#[test]
fn render_state_frames_reach_every_view() {
    // libghostty-vt's render state tracks changes once; each view still gets every row that
    // changed since its own last frame.
    let mut t = term(20, 4);
    let (mut a, mut b) = (ClientView::new(), ClientView::new());
    t.advance(b"one");
    t.snapshot(1, &mut a).unwrap();
    t.snapshot(1, &mut b).unwrap();
    t.advance(b"\r\ntwo");
    let fa = t.snapshot(1, &mut a).unwrap();
    assert_eq!(
        fa.frame.lines.iter().map(|l| l.row).collect::<Vec<_>>(),
        vec![1]
    );
    t.advance(b"\r\nthree");
    // `b` hasn't seen "two" or "three"; `a` consumed the change to row 1 already.
    let fb = t.snapshot(1, &mut b).unwrap();
    let mut rows: Vec<u16> = fb.frame.lines.iter().map(|l| l.row).collect();
    rows.sort();
    assert_eq!(rows, vec![1, 2]);
    assert_eq!(row_text(&fb.frame, 2), "three");
    let fa = t.snapshot(1, &mut a).unwrap();
    assert_eq!(
        fa.frame.lines.iter().map(|l| l.row).collect::<Vec<_>>(),
        vec![2]
    );
    assert!(t.snapshot(1, &mut a).is_none(), "nothing new");
}

#[test]
fn cursor_style_is_libghosttys() {
    let size = PaneSize {
        cols: 20,
        rows: 4,
        cell_width: 10,
        cell_height: 20,
    };
    let cfg = EngineConfig {
        cursor_style: thurm_config::CursorStyle::Beam,
        cursor_blink: true,
        ..EngineConfig::default()
    };
    let mut t = Terminal::new(size, cfg);
    let cursor = |t: &mut Terminal| {
        let c = t.snapshot(1, &mut ClientView::new()).unwrap().frame.cursor;
        (c.shape, c.blinking)
    };
    // The configured default, a program's DECSCUSR, and back to the default.
    assert_eq!(cursor(&mut t), (CursorShape::Beam, true));
    t.advance(b"\x1b[4 q");
    assert_eq!(cursor(&mut t), (CursorShape::Underline, false));
    t.advance(b"\x1b[0 q");
    assert_eq!(cursor(&mut t), (CursorShape::Beam, true));
    t.advance(b"\x1b[?25l");
    assert_eq!(cursor(&mut t).0, CursorShape::Hidden);
    // A copy attached later gets the program's style.
    t.advance(b"\x1b[?25h\x1b[1 q");
    let mut copy = term(20, 4);
    copy.replay(&t.serialize_state());
    assert_eq!(cursor(&mut copy), (CursorShape::Block, true));
}

#[test]
fn color_scheme_reports() {
    let mut t = term(20, 4);
    let mut cfg = EngineConfig {
        dark: true,
        ..EngineConfig::default()
    };
    t.set_config(cfg.clone());
    t.drain_events();
    t.advance(b"\x1b[?996n");
    assert_eq!(responses(&mut t), "\x1b[?997;1n");
    // Change notifications, once a program asks for them.
    t.advance(b"\x1b[?2031h");
    cfg.dark = false;
    t.set_config(cfg);
    assert_eq!(responses(&mut t), "\x1b[?997;2n");
}

#[test]
fn shell_path_needs_the_pane_token() {
    let mut t = term(40, 5);
    // Without a token (program output, or a copy of the terminal) nothing is accepted.
    t.advance(b"\x1b]633;P;ThurmPath=/usr/bin\x07");
    assert_eq!(t.shell_path(), None);
    t.set_shell_token(Some("secret".into()));
    // `cat README` printing a PATH of its own.
    t.advance(b"\x1b]633;P;ThurmPath=.\x07");
    t.advance(b"\x1b]633;P;ThurmPath=wrong:/tmp/evil\x07");
    assert_eq!(t.shell_path(), None);
    // The shell's report; relative entries are dropped.
    t.advance(b"\x1b]633;P;ThurmPath=secret:.:/opt/bin::bin:/usr/bin\x07");
    assert_eq!(t.shell_path(), Some("/opt/bin:/usr/bin"));
}

#[test]
fn osc52_is_forwarded_once() {
    let mut t = term(40, 5);
    t.set_config(EngineConfig {
        osc52: thurm_config::Osc52Mode::CopyPaste,
        ..EngineConfig::default()
    });
    let _ = t.drain_events();
    let data = t.advance_forward(b"\x1b]52;c;aGVsbG8=\x07");
    let text = String::from_utf8_lossy(&data);
    assert_eq!(text.matches("]52;").count(), 1, "{text:?}");
    let stores = t
        .drain_events()
        .iter()
        .filter(|e| matches!(e, TermEvent::ClipboardStore(_)))
        .count();
    assert_eq!(stores, 1);
}
