//! Color themes: Thurm's own, plus a collection of several hundred (from iTerm2-Color-Schemes,
//! MIT, as Ghostty bundles them) embedded from `themes/`. Colors are `0xRRGGBB`.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub cursor_text: u32,
    pub selection_foreground: u32,
    pub selection_background: u32,
    /// ANSI 0-15.
    pub palette: [u32; 16],
}

/// File format for user themes: same fields, colors as strings.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    name: Option<String>,
    foreground: String,
    background: String,
    cursor: Option<String>,
    cursor_text: Option<String>,
    selection_foreground: Option<String>,
    selection_background: Option<String>,
    palette: Vec<String>,
}

impl Theme {
    /// Whether the background is dark (relative luminance below 0.5).
    pub fn is_dark(&self) -> bool {
        let lin = |c: u32| {
            let v = (c & 0xff) as f64 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let bg = self.background;
        let l = 0.2126 * lin(bg >> 16) + 0.7152 * lin(bg >> 8) + 0.0722 * lin(bg);
        l < 0.5
    }

    /// A theme file: Thurm's TOML, or the collection's `key = value` format.
    pub fn load_file(path: &Path) -> Result<Theme, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let f: ThemeFile = match toml::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                let name = path.display().to_string();
                return Theme::parse_collection(&name, &text).map_err(|_| e.to_string());
            }
        };
        let c = |s: &str| crate::parse_color(s).ok_or_else(|| format!("bad color {s:?}"));
        let fg = c(&f.foreground)?;
        let bg = c(&f.background)?;
        if f.palette.len() != 16 {
            return Err("palette needs 16 colors".into());
        }
        let mut palette = [0; 16];
        for (i, p) in f.palette.iter().enumerate() {
            palette[i] = c(p)?;
        }
        let opt =
            |s: &Option<String>, d: u32| s.as_deref().map(c).transpose().map(|v| v.unwrap_or(d));
        Ok(Theme {
            name: f.name.unwrap_or_else(|| path.display().to_string()),
            foreground: fg,
            background: bg,
            cursor: opt(&f.cursor, fg)?,
            cursor_text: opt(&f.cursor_text, bg)?,
            selection_foreground: opt(&f.selection_foreground, bg)?,
            selection_background: opt(&f.selection_background, fg)?,
            palette,
        })
    }
}

impl Theme {
    /// The collection's format: `palette = N=#rrggbb`, `background = #rrggbb`, `foreground`,
    /// `cursor-color`, `cursor-text`, `selection-background`, `selection-foreground`. Missing
    /// palette entries come from Thurm's default theme.
    pub fn parse_collection(name: &str, text: &str) -> Result<Theme, String> {
        let base = builtin(crate::DEFAULT_THEME).expect("default theme");
        let color =
            |v: &str| crate::parse_color(v.trim()).ok_or_else(|| format!("bad color {v:?}"));
        let (mut fg, mut bg) = (None, None);
        let (mut cursor, mut cursor_text, mut sel_fg, mut sel_bg) = (None, None, None, None);
        let mut palette = base.palette;
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "palette" => {
                    let (i, c) = value
                        .split_once('=')
                        .ok_or_else(|| format!("bad palette entry {value:?}"))?;
                    let i: usize = i
                        .trim()
                        .parse()
                        .map_err(|_| format!("bad palette index {i:?}"))?;
                    if i < 16 {
                        palette[i] = color(c)?;
                    }
                }
                "background" => bg = Some(color(value)?),
                "foreground" => fg = Some(color(value)?),
                "cursor-color" => cursor = Some(color(value)?),
                "cursor-text" => cursor_text = Some(color(value)?),
                "selection-background" => sel_bg = Some(color(value)?),
                "selection-foreground" => sel_fg = Some(color(value)?),
                _ => {}
            }
        }
        let fg = fg.ok_or("no foreground")?;
        let bg = bg.ok_or("no background")?;
        Ok(Theme {
            name: name.to_owned(),
            foreground: fg,
            background: bg,
            cursor: cursor.unwrap_or(fg),
            cursor_text: cursor_text.unwrap_or(bg),
            selection_foreground: sel_fg.unwrap_or(bg),
            selection_background: sel_bg.unwrap_or(fg),
            palette,
        })
    }
}

mod collection {
    include!(concat!(env!("OUT_DIR"), "/themes.rs"));
}

macro_rules! theme {
    ($name:expr, fg $fg:expr, bg $bg:expr, cursor $cur:expr, ctext $ct:expr, sfg $sfg:expr, sbg $sbg:expr, [$($p:expr),* $(,)?]) => {
        Theme {
            name: $name.to_string(),
            foreground: $fg,
            background: $bg,
            cursor: $cur,
            cursor_text: $ct,
            selection_foreground: $sfg,
            selection_background: $sbg,
            palette: [$($p),*],
        }
    };
}

fn all() -> Vec<Theme> {
    vec![
        // Default: cool steel on near-black, with a liquid-metal accent.
        theme!("thurm", fg 0xd6dde6, bg 0x0e1116, cursor 0x9fb4c8, ctext 0x0e1116, sfg 0x0e1116, sbg 0x9fb4c8, [
            0x1b2029, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xc8d0da,
            0x4b5263, 0xff7b86, 0xb5e890, 0xffd68a, 0x82c4ff, 0xde9bf0, 0x7fd3de, 0xffffff,
        ]),
        theme!("thurm-light", fg 0x24292f, bg 0xfafbfc, cursor 0x3b5b7a, ctext 0xfafbfc, sfg 0x24292f, sbg 0xc8d9ea, [
            0x24292f, 0xcf222e, 0x116329, 0x7d4e00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
            0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
        ]),
        theme!("tokyo-night", fg 0xc0caf5, bg 0x1a1b26, cursor 0xc0caf5, ctext 0x1a1b26, sfg 0xc0caf5, sbg 0x33467c, [
            0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
            0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
        ]),
        theme!("catppuccin-mocha", fg 0xcdd6f4, bg 0x1e1e2e, cursor 0xf5e0dc, ctext 0x1e1e2e, sfg 0x1e1e2e, sbg 0xf5e0dc, [
            0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
            0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
        ]),
        theme!("catppuccin-latte", fg 0x4c4f69, bg 0xeff1f5, cursor 0xdc8a78, ctext 0xeff1f5, sfg 0xeff1f5, sbg 0xdc8a78, [
            0x5c5f77, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xacb0be,
            0x6c6f85, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xbcc0cc,
        ]),
        theme!("gruvbox-dark", fg 0xebdbb2, bg 0x282828, cursor 0xebdbb2, ctext 0x282828, sfg 0x282828, sbg 0xebdbb2, [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ]),
        theme!("dracula", fg 0xf8f8f2, bg 0x282a36, cursor 0xf8f8f2, ctext 0x282a36, sfg 0xf8f8f2, sbg 0x44475a, [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ]),
        theme!("nord", fg 0xd8dee9, bg 0x2e3440, cursor 0xd8dee9, ctext 0x2e3440, sfg 0xd8dee9, sbg 0x434c5e, [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
            0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
        ]),
        theme!("solarized-dark", fg 0x839496, bg 0x002b36, cursor 0x93a1a1, ctext 0x002b36, sfg 0x93a1a1, sbg 0x073642, [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
            0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
        ]),
    ]
}

/// One of Thurm's own themes.
fn builtin(name: &str) -> Option<Theme> {
    all()
        .into_iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
}

/// A theme by name: Thurm's own first, then the collection (names as its files, e.g.
/// "Catppuccin Mocha"; case doesn't matter).
pub fn builtin_theme(name: &str) -> Option<Theme> {
    builtin(name).or_else(|| {
        let (n, text) = collection::THEMES
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))?;
        Theme::parse_collection(n, text).ok()
    })
}

/// Thurm's own themes, then the collection (alphabetical).
pub fn builtin_theme_names() -> Vec<String> {
    all()
        .into_iter()
        .map(|t| t.name)
        .chain(collection::THEMES.iter().map(|(n, _)| (*n).to_owned()))
        .collect()
}

/// Whether `name` is one of Thurm's own themes (not from the collection).
pub fn is_own_theme(name: &str) -> bool {
    builtin(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_collection_theme_parses() {
        assert!(collection::THEMES.len() > 500);
        for (name, text) in collection::THEMES {
            let t = Theme::parse_collection(name, text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(&t.name, name);
        }
    }

    #[test]
    fn lookup_by_name() {
        let t = builtin_theme("catppuccin mocha").expect("collection theme, any case");
        assert_eq!(
            (t.name.as_str(), t.background, t.palette[1]),
            ("Catppuccin Mocha", 0x1e1e2e, 0xf38ba8)
        );
        assert_eq!(t.cursor, 0xf5e0dc);
        // Own themes win and keep their names.
        assert_eq!(
            builtin_theme("catppuccin-mocha").unwrap().name,
            "catppuccin-mocha"
        );
        assert!(is_own_theme("thurm") && !is_own_theme("Dracula+"));
        assert!(builtin_theme("no such theme").is_none());
    }
}
