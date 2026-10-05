//! The app's view of `config.toml`: the parsed config with the clamps the macOS app applies
//! (Config.swift), the theme resolved for the current appearance, and load errors/warnings.

use thurm_config::{Config, Theme, ThemeInfo, ThemeSpec};
use thurm_proto::AgentPreset;

pub struct UiConfig {
    pub cfg: Config,
    pub theme: Theme,
    pub theme_spec: ThemeSpec,
    pub themes: Vec<ThemeInfo>,
    pub font_features: Vec<String>,
    pub presets: Vec<AgentPreset>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
    pub dark: bool,
}

impl UiConfig {
    pub fn load(dark: bool) -> UiConfig {
        let _ = thurm_config::ensure_default_config();
        let (cfg, error, warnings) = match Config::load_with_warnings() {
            Ok((cfg, w)) => (cfg, None, w),
            Err(e) => (Config::default(), Some(e.to_string()), Vec::new()),
        };
        UiConfig {
            theme: cfg.theme_for(dark),
            theme_spec: cfg.theme_spec(),
            themes: thurm_config::theme_list(),
            font_features: cfg.font_features(),
            presets: cfg.agent_presets(),
            cfg,
            error,
            warnings,
            dark,
        }
    }

    /// The toast shown after loading: an error, else a warnings summary.
    pub fn problem(&self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(format!("Config error: {e}"));
        }
        let first = self.warnings.first()?;
        Some(if self.warnings.len() > 1 {
            format!("Config: {first} (+{} more)", self.warnings.len() - 1)
        } else {
            format!("Config: {first}")
        })
    }

    pub fn padding(&self) -> (f64, f64) {
        (
            self.cfg.window.padding_x.clamp(0.0, 200.0),
            self.cfg.window.padding_y.clamp(0.0, 200.0),
        )
    }

    pub fn opacity(&self) -> f64 {
        self.cfg.window.opacity.clamp(0.05, 1.0)
    }

    /// The quick terminal's background: `quick_terminal.opacity`, else the window's.
    pub fn quick_opacity(&self) -> f64 {
        self.cfg.quick_terminal.opacity.map_or(self.opacity(), |o| o.clamp(0.05, 1.0))
    }

    pub fn font_size(&self) -> f64 {
        self.cfg.font.size.clamp(4.0, 200.0)
    }

    pub fn unfocused_dim(&self) -> f64 {
        self.cfg.window.unfocused_split_dim.clamp(0.0, 1.0)
    }

    pub fn scroll_multiplier(&self) -> f64 {
        self.cfg.terminal.scroll_multiplier.clamp(0.1, 100.0)
    }

    pub fn cursor_thickness(&self) -> f64 {
        self.cfg.cursor.thickness.clamp(0.0, 20.0)
    }

    pub fn sidebar_tabs(&self) -> bool {
        matches!(self.cfg.window.tab_style, thurm_config::TabStyle::Sidebar)
    }

    pub fn sidebar_width(&self) -> f64 {
        self.cfg.window.sidebar_width.clamp(180.0, 480.0)
    }

    pub fn ai_explain(&self) -> bool {
        self.cfg.ai.enabled && self.cfg.ai.explain
    }

    pub fn follows_appearance(&self) -> bool {
        self.theme_spec.follows_appearance()
    }

    /// Theme darkness from its background's luminance (window chrome follows the terminal
    /// theme, not the system).
    pub fn theme_is_dark(&self) -> bool {
        luminance(self.theme.background) < 0.5
    }

    pub fn remote_names(&self) -> Vec<String> {
        self.cfg.remote.iter().map(|r| r.name.clone()).collect()
    }
}

/// Relative luminance of 0xRRGGBB.
pub fn luminance(c: u32) -> f64 {
    let lin = |v: u32| {
        let v = v as f64 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin((c >> 16) & 0xff) + 0.7152 * lin((c >> 8) & 0xff) + 0.0722 * lin(c & 0xff)
}

/// `a` moved `t` of the way to `b`.
pub fn blend(a: u32, b: u32, t: f64) -> u32 {
    let ch = |s: u32| {
        let x = ((a >> s) & 0xff) as f64;
        let y = ((b >> s) & 0xff) as f64;
        ((x + (y - x) * t).round() as u32) & 0xff
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

pub fn css_rgb(c: u32) -> String {
    format!("#{:06x}", c & 0xff_ffff)
}

pub fn css_rgba(c: u32, a: f64) -> String {
    format!(
        "rgba({},{},{},{:.3})",
        (c >> 16) & 0xff,
        (c >> 8) & 0xff,
        c & 0xff,
        a
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        assert!(luminance(0x000000) < 0.01);
        assert!(luminance(0xffffff) > 0.99);
        assert_eq!(blend(0x000000, 0xffffff, 0.25), 0x404040);
        assert_eq!(css_rgb(0x1e1e2e), "#1e1e2e");
    }
}
