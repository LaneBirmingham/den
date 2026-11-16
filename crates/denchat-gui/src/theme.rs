//! Themes: a small TOML palette mapped onto egui's style.
//!
//! Panels never hard-code colours; they ask the [`Palette`] for tokens
//! (`accent`, `muted`, `danger`, …). Mods can use the same tokens in widget
//! `color` fields, so a reskin restyles mod panels too.

use std::path::{Path, PathBuf};

use anyhow::Context;
use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Theme {
    pub name: String,
    pub dark: bool,
    pub colors: Colors,
    pub shape: Shape,
    pub text: Text,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Colors {
    pub bg: String,
    pub panel: String,
    pub surface: String,
    pub surface_hover: String,
    pub border: String,
    pub text: String,
    pub muted: String,
    pub accent: String,
    pub accent_text: String,
    pub ok: String,
    pub warn: String,
    pub danger: String,
    pub speaking: String,
    pub link: String,
    pub mention: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Shape {
    pub rounding: u8,
    pub spacing: f32,
    pub padding: f32,
    pub border_width: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Text {
    pub size: f32,
    pub heading_size: f32,
    pub small_size: f32,
    /// Use the monospace family everywhere.
    pub monospace: bool,
    /// Optional font files, relative to the theme file.
    pub font: Option<String>,
    pub mono_font: Option<String>,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            name: crate::app::DEFAULT_THEME.into(),
            dark: true,
            colors: Colors::default(),
            shape: Shape::default(),
            text: Text::default(),
        }
    }
}

impl Default for Colors {
    fn default() -> Self {
        let s = |v: &str| v.to_owned();
        Colors {
            // catppuccin (the default theme), so a partial theme file fills in sensibly.
            bg: s("#1e1e2e"),
            panel: s("#181825"),
            surface: s("#313244"),
            surface_hover: s("#45475a"),
            border: s("#313244"),
            text: s("#cdd6f4"),
            muted: s("#9399b2"),
            accent: s("#cba6f7"),
            accent_text: s("#1e1e2e"),
            ok: s("#a6e3a1"),
            warn: s("#f9e2af"),
            danger: s("#f38ba8"),
            speaking: s("#a6e3a1"),
            link: s("#89b4fa"),
            mention: s("#3a3050"),
        }
    }
}

impl Default for Shape {
    fn default() -> Self {
        Shape { rounding: 6, spacing: 6.0, padding: 10.0, border_width: 1.0 }
    }
}

impl Default for Text {
    fn default() -> Self {
        Text { size: 14.5, heading_size: 17.0, small_size: 12.0, monospace: false, font: None, mono_font: None }
    }
}

/// Resolved colours for this frame: the theme's tokens plus shades derived
/// from them (so theme files never need more keys than the ones above).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub panel: Color32,
    pub surface: Color32,
    pub surface_hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub speaking: Color32,
    pub link: Color32,
    pub mention: Color32,
    pub rounding: u8,
    pub padding: f32,
    pub size: f32,
    pub heading_size: f32,
    pub small_size: f32,

    // ----- derived ----------------------------------------------------------
    /// Behind the cards (darker than `panel`).
    pub canvas: Color32,
    /// Inset areas inside a card.
    pub sunk: Color32,
    /// Hairlines and dividers.
    pub line: Color32,
    pub line_strong: Color32,
    /// Translucent overlays for hovered / selected rows.
    pub row_hover: Color32,
    pub row_active: Color32,
    /// Body text (a touch softer than `text`, which stays for names and titles).
    pub text2: Color32,
    pub accent_soft: Color32,
    /// Accent that stays readable as text on light themes.
    pub accent_ink: Color32,
    pub danger_soft: Color32,
    pub ok_soft: Color32,
    /// Other people's chat bubbles.
    pub bubble: Color32,
    pub shadow: Shadow,
    /// Big containers (the shell's cards).
    pub card_radius: u8,
    /// Rows, buttons and small cards.
    pub row_radius: u8,
}

/// Blend `a` toward `b` by `t` (sRGB, like CSS `color-mix`).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    // Color32 is premultiplied, so lerping all four channels is correct for translucent colours too.
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// `c` at opacity `a` (0..=1).
pub fn alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// `#RGB`, `#RRGGBB` or `#RRGGBBAA`.
pub fn parse_color(s: &str) -> Option<Color32> {
    let h = s.trim().strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    match h.len() {
        3 => {
            let n = |i: usize| u8::from_str_radix(&h[i..=i], 16).ok().map(|v| v * 17);
            Some(Color32::from_rgb(n(0)?, n(1)?, n(2)?))
        }
        6 => Some(Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

impl Palette {
    /// Placeholder values, overwritten by [`Palette::derived`].
    fn derive_defaults() -> Palette {
        let z = Color32::TRANSPARENT;
        Palette {
            dark: true,
            bg: z,
            panel: z,
            surface: z,
            surface_hover: z,
            border: z,
            text: z,
            muted: z,
            accent: z,
            accent_text: z,
            ok: z,
            warn: z,
            danger: z,
            speaking: z,
            link: z,
            mention: z,
            rounding: 6,
            padding: 10.0,
            size: 14.5,
            heading_size: 17.0,
            small_size: 12.0,
            canvas: z,
            sunk: z,
            line: z,
            line_strong: z,
            row_hover: z,
            row_active: z,
            text2: z,
            accent_soft: z,
            accent_ink: z,
            danger_soft: z,
            ok_soft: z,
            bubble: z,
            shadow: Shadow::NONE,
            card_radius: 12,
            row_radius: 8,
        }
    }

    /// Fill in the derived shades from the theme's tokens.
    fn derived(mut self) -> Palette {
        let d = self.dark;
        let black = Color32::BLACK;
        self.canvas = mix(self.panel, black, if d { 0.3 } else { 0.07 });
        self.sunk = mix(self.panel, black, if d { 0.14 } else { 0.035 });
        self.line = mix(self.bg, self.text, if d { 0.09 } else { 0.11 });
        self.line_strong = mix(self.bg, self.text, if d { 0.16 } else { 0.2 });
        self.row_hover = alpha(self.text, if d { 0.045 } else { 0.05 });
        self.row_active = alpha(self.text, 0.09);
        self.text2 = mix(self.text, self.bg, 0.08);
        self.accent_soft = alpha(self.accent, if d { 0.16 } else { 0.18 });
        self.accent_ink = if d { self.accent } else { mix(self.accent, black, 0.15) };
        self.danger_soft = alpha(self.danger, if d { 0.16 } else { 0.13 });
        self.ok_soft = alpha(self.ok, 0.16);
        self.bubble = if d { self.surface } else { mix(self.surface, self.text, 0.07) };
        self.shadow = Shadow {
            offset: [0, 4],
            blur: 20,
            spread: 0,
            color: if d { Color32::from_black_alpha(80) } else { Color32::from_rgba_unmultiplied(60, 40, 30, 22) },
        };
        let r = self.rounding as f32;
        self.card_radius = (r * 1.5 + 2.0).clamp(4.0, 18.0) as u8;
        self.row_radius = (r + 2.0).clamp(3.0, 12.0) as u8;
        self
    }

    /// A colour by token name (`accent`, `muted`, …) or hex.
    pub fn token(&self, name: &str) -> Option<Color32> {
        Some(match name {
            "bg" => self.bg,
            "panel" => self.panel,
            "surface" => self.surface,
            "border" => self.border,
            "text" => self.text,
            "muted" | "dim" => self.muted,
            "accent" => self.accent,
            "ok" | "success" | "green" => self.ok,
            "warn" | "warning" | "yellow" => self.warn,
            "danger" | "error" | "red" => self.danger,
            "link" => self.link,
            "speaking" => self.speaking,
            other => return parse_color(other),
        })
    }
}

impl Theme {
    pub fn parse(text: &str) -> anyhow::Result<Theme> {
        let t: Theme = toml::from_str(text)?;
        let p = t.palette_checked()?;
        let _ = p;
        Ok(t)
    }

    pub fn load(path: &Path) -> anyhow::Result<Theme> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Theme::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    fn palette_checked(&self) -> anyhow::Result<Palette> {
        let c = &self.colors;
        let get = |name: &str, v: &str| parse_color(v).with_context(|| format!("colors.{name} = {v:?} is not #RRGGBB"));
        Ok(Palette {
            dark: self.dark,
            bg: get("bg", &c.bg)?,
            panel: get("panel", &c.panel)?,
            surface: get("surface", &c.surface)?,
            surface_hover: get("surface_hover", &c.surface_hover)?,
            border: get("border", &c.border)?,
            text: get("text", &c.text)?,
            muted: get("muted", &c.muted)?,
            accent: get("accent", &c.accent)?,
            accent_text: get("accent_text", &c.accent_text)?,
            ok: get("ok", &c.ok)?,
            warn: get("warn", &c.warn)?,
            danger: get("danger", &c.danger)?,
            speaking: get("speaking", &c.speaking)?,
            link: get("link", &c.link)?,
            mention: get("mention", &c.mention)?,
            rounding: self.shape.rounding,
            padding: self.shape.padding,
            size: self.text.size,
            heading_size: self.text.heading_size,
            small_size: self.text.small_size,
            ..Palette::derive_defaults()
        }
        .derived())
    }

    pub fn palette(&self) -> Palette {
        self.palette_checked().unwrap_or_else(|_| Theme::default().palette_checked().expect("default theme is valid"))
    }

    /// Push this theme into egui. `base` is the theme file's directory (for fonts).
    pub fn apply(&self, ctx: &egui::Context, base: Option<&Path>) {
        let p = self.palette();
        let mut v = if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        let r = CornerRadius::same(p.row_radius);
        let bw = self.shape.border_width;
        v.dark_mode = self.dark;
        v.override_text_color = Some(p.text);
        v.panel_fill = p.panel;
        v.window_fill = p.panel;
        v.window_stroke = Stroke::new(bw, p.line);
        v.window_corner_radius = CornerRadius::same(p.card_radius);
        v.window_shadow = p.shadow;
        v.popup_shadow = p.shadow;
        v.menu_corner_radius = r;
        v.extreme_bg_color = p.surface;
        v.text_edit_bg_color = Some(p.surface);
        v.faint_bg_color = p.surface;
        v.code_bg_color = p.surface;
        v.hyperlink_color = p.link;
        v.warn_fg_color = p.warn;
        v.error_fg_color = p.danger;
        v.selection.bg_fill = p.accent.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0, p.accent);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = p.panel;
        w.noninteractive.weak_bg_fill = p.panel;
        w.noninteractive.bg_stroke = Stroke::new(bw, p.line);
        w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
        w.noninteractive.corner_radius = r;
        for (state, fill) in [
            (&mut w.inactive, p.surface),
            (&mut w.hovered, p.surface_hover),
            (&mut w.active, p.surface_hover),
            (&mut w.open, p.surface_hover),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.bg_stroke = Stroke::NONE;
            state.fg_stroke = Stroke::new(1.0, p.text);
            state.corner_radius = r;
            state.expansion = 0.0;
        }
        w.inactive.bg_stroke = Stroke::new(bw, p.border);
        w.hovered.bg_stroke = Stroke::new(bw, p.accent.gamma_multiply(0.6));
        w.active.bg_stroke = Stroke::new(bw, p.accent);

        let size = self.text.size;
        let prop = if self.text.monospace { FontFamily::Monospace } else { FontFamily::Proportional };
        ctx.all_styles_mut(|style| {
            style.visuals = v.clone();
            style.spacing.item_spacing = egui::vec2(self.shape.spacing, self.shape.spacing * 0.75);
            style.spacing.button_padding = egui::vec2(self.shape.spacing + 2.0, self.shape.spacing * 0.5 + 1.0);
            style.spacing.window_margin = Margin::same(self.shape.padding as i8);
            style.spacing.menu_margin = Margin::same((self.shape.padding * 0.6) as i8);
            style.spacing.scroll = egui::style::ScrollStyle::floating();
            style.spacing.scroll.bar_width = 8.0;
            style.spacing.scroll.floating_allocated_width = 0.0;
            style.interaction.selectable_labels = false;
            style.text_styles = [
                (TextStyle::Small, FontId::new(self.text.small_size, prop.clone())),
                (TextStyle::Body, FontId::new(size, prop.clone())),
                (TextStyle::Button, FontId::new(size, prop.clone())),
                (TextStyle::Heading, FontId::new(self.text.heading_size, prop.clone())),
                (TextStyle::Monospace, FontId::new(size - 0.5, FontFamily::Monospace)),
            ]
            .into();
        });
        ctx.set_fonts(self.fonts(base));
    }

    fn fonts(&self, base: Option<&Path>) -> FontDefinitions {
        let mut defs = FontDefinitions::default();
        // egui's fallbacks (Ubuntu, emoji) stay after the bundled fonts for anything Inter lacks.
        let fallbacks = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
        let mono = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
        for (key, bytes) in crate::assets::FONTS {
            defs.font_data.insert((*key).into(), std::sync::Arc::new(FontData::from_static(bytes)));
        }
        let chain = |first: &str| -> Vec<String> {
            let mut v = vec![first.to_owned(), "phosphor".to_owned()];
            v.extend(fallbacks.iter().cloned());
            v
        };
        let mut mono_chain = mono.clone();
        mono_chain.insert(mono_chain.len().min(1), "phosphor".into());
        if self.text.monospace {
            for f in [FontFamily::Proportional, crate::kit::MEDIUM.clone(), crate::kit::SEMIBOLD.clone()] {
                defs.families.insert(f, mono_chain.clone());
            }
        } else {
            defs.families.insert(FontFamily::Proportional, chain("inter"));
            defs.families.insert(crate::kit::MEDIUM.clone(), chain("inter-medium"));
            defs.families.insert(crate::kit::SEMIBOLD.clone(), chain("inter-semibold"));
        }
        defs.families.insert(FontFamily::Monospace, mono_chain);
        let resolve = |rel: &str| -> PathBuf {
            let p = Path::new(rel);
            match base {
                Some(b) if p.is_relative() => b.join(p),
                _ => p.to_path_buf(),
            }
        };
        let text_families = [FontFamily::Proportional, crate::kit::MEDIUM.clone(), crate::kit::SEMIBOLD.clone()];
        for (key, families, rel) in
            [("theme-font", &text_families[..], &self.text.font), ("theme-mono", &[FontFamily::Monospace][..], &self.text.mono_font)]
        {
            let Some(rel) = rel.as_deref().filter(|r| !r.is_empty()) else { continue };
            let path = resolve(rel);
            match std::fs::read(&path) {
                Ok(bytes) => {
                    defs.font_data.insert(key.into(), std::sync::Arc::new(FontData::from_owned(bytes)));
                    // A theme font replaces every weight (one file has one weight).
                    for family in families {
                        defs.families.entry(family.clone()).or_default().insert(0, key.into());
                    }
                }
                Err(e) => tracing::warn!("theme font {}: {e}", path.display()),
            }
        }
        defs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        assert_eq!(parse_color("#fff"), Some(Color32::WHITE));
        assert_eq!(parse_color("#000000"), Some(Color32::BLACK));
        assert!(parse_color("red").is_none());
        assert!(parse_color("#12345").is_none());
    }

    #[test]
    fn builtin_themes_parse() {
        for (name, text) in crate::assets::THEMES {
            let t = Theme::parse(text).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(&t.name, name);
        }
    }

    #[test]
    fn mix_keeps_translucency() {
        let soft = alpha(Color32::from_rgb(200, 100, 50), 0.2);
        assert_eq!(mix(soft, Color32::WHITE, 0.0), soft);
        assert!(mix(soft, Color32::from_rgb(200, 100, 50), 0.5).a() > soft.a());
    }

    #[test]
    fn bad_color_is_an_error() {
        assert!(Theme::parse("[colors]\naccent = \"blue\"").is_err());
    }
}
