//! The WinUI theme colours windows-reactor's [`ThemeBrush`] does not name, for the Windows
//! client shell and the installer wizard.
//!
//! The values are WinUI's own (`Common_themeresources_any.xaml`, Light and Default
//! dictionaries). They are static colours there, so a [`Palette`] picks them by colour scheme.
//! Attention follows the system accent, as WinUI's brush does. Under high contrast every token
//! falls back to the nearest [`ThemeBrush`], which WinUI resolves to the user's system colours.
//! [`motion`] carries the timer-driven tween both apps animate with.
#![cfg(windows)]

pub mod motion;

use windows::UI::ViewManagement::{AccessibilitySettings, UIColorType, UISettings};
use windows_reactor::{Brush, Color, ColorScheme, ThemeBrush};

/// A theme colour by its WinUI role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    SecondaryText,
    TertiaryText,
    TextOnAccent,
    SubtleFill,
    ControlFillSecondary,
    DividerStroke,
    SurfaceStroke,
    SystemAttention,
    SystemAttentionBackground,
    SystemSuccess,
    SystemSuccessBackground,
    SystemCaution,
    SystemNeutral,
}

/// The colours one window paints with: its scheme, the system accent and high contrast.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub scheme: ColorScheme,
    high_contrast: bool,
    accent: Color,
    accent_light2: Color,
}

const fn argb(v: u32) -> Color {
    Color::argb((v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// `(light, dark)` as WinUI's ARGB.
fn table(t: Token) -> (u32, u32) {
    match t {
        Token::SecondaryText => (0x9E00_0000, 0xC5FF_FFFF),
        Token::TertiaryText => (0x7200_0000, 0x87FF_FFFF),
        Token::TextOnAccent => (0xFFFF_FFFF, 0xFF00_0000),
        Token::SubtleFill => (0x0900_0000, 0x0FFF_FFFF),
        Token::ControlFillSecondary => (0x80F9_F9F9, 0x15FF_FFFF),
        Token::DividerStroke => (0x0F00_0000, 0x15FF_FFFF),
        Token::SurfaceStroke => (0x6675_7575, 0x6675_7575),
        Token::SystemAttentionBackground => (0x80F6_F6F6, 0x08FF_FFFF),
        Token::SystemSuccess => (0xFF0F_7B0F, 0xFF6C_CB5F),
        Token::SystemSuccessBackground => (0xFFDF_F6DD, 0xFF39_3D1B),
        Token::SystemCaution => (0xFF9D_5D00, 0xFFFC_E100),
        Token::SystemNeutral => (0x7200_0000, 0x8BFF_FFFF),
        // Accent-derived; `get` answers it before the table.
        Token::SystemAttention => (0, 0),
    }
}

/// The theme brush a token stands in for under high contrast.
fn high_contrast(t: Token) -> ThemeBrush {
    match t {
        Token::SecondaryText
        | Token::TertiaryText
        | Token::SystemNeutral
        | Token::SystemSuccess
        | Token::SystemCaution => ThemeBrush::PrimaryText,
        Token::TextOnAccent => ThemeBrush::SolidBackground,
        Token::SystemAttention => ThemeBrush::AccentText,
        Token::DividerStroke | Token::SurfaceStroke => ThemeBrush::CardStroke,
        Token::SubtleFill
        | Token::ControlFillSecondary
        | Token::SystemAttentionBackground
        | Token::SystemSuccessBackground => ThemeBrush::CardBackground,
    }
}

impl Palette {
    /// The system's current scheme, accent and contrast. A refused query reads as light,
    /// WinUI's default blue and no high contrast.
    pub fn system() -> Palette {
        let ui = UISettings::new().ok();
        let color = |kind: UIColorType| {
            ui.as_ref()
                .and_then(|s| s.GetColorValue(kind).ok())
                .map(|c| Color::argb(c.A, c.R, c.G, c.B))
        };
        // App mode dark paints a dark background.
        let scheme = match color(UIColorType::Background) {
            Some(bg) if u16::from(bg.r) + u16::from(bg.g) + u16::from(bg.b) < 384 => {
                ColorScheme::Dark
            }
            _ => ColorScheme::Light,
        };
        Palette {
            scheme,
            high_contrast: AccessibilitySettings::new()
                .and_then(|a| a.HighContrast())
                .unwrap_or(false),
            accent: color(UIColorType::Accent).unwrap_or(Color::rgb(0x00, 0x5F, 0xB8)),
            accent_light2: color(UIColorType::AccentLight2).unwrap_or(Color::rgb(0x60, 0xCD, 0xFF)),
        }
    }

    /// The same palette for the scheme a window just reported.
    pub fn with_scheme(self, scheme: ColorScheme) -> Palette {
        Palette { scheme, ..self }
    }

    pub fn get(&self, t: Token) -> Brush {
        if self.high_contrast {
            return high_contrast(t).into();
        }
        let dark = self.scheme == ColorScheme::Dark;
        if t == Token::SystemAttention {
            return if dark {
                self.accent_light2
            } else {
                self.accent
            }
            .into();
        }
        let (light, dark_v) = table(t);
        argb(if dark { dark_v } else { light }).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette(scheme: ColorScheme, high_contrast: bool) -> Palette {
        Palette {
            scheme,
            high_contrast,
            accent: Color::rgb(1, 2, 3),
            accent_light2: Color::rgb(4, 5, 6),
        }
    }

    #[test]
    fn tokens_read_by_scheme_and_contrast() {
        let light = palette(ColorScheme::Light, false);
        let dark = light.with_scheme(ColorScheme::Dark);
        assert_eq!(
            light.get(Token::SecondaryText),
            Brush::Solid(Color::argb(0x9E, 0, 0, 0))
        );
        assert_eq!(
            dark.get(Token::SecondaryText),
            Brush::Solid(Color::argb(0xC5, 0xFF, 0xFF, 0xFF))
        );
        assert_eq!(
            light.get(Token::SystemAttention),
            Brush::Solid(Color::rgb(1, 2, 3))
        );
        assert_eq!(
            dark.get(Token::SystemAttention),
            Brush::Solid(Color::rgb(4, 5, 6))
        );
        let hc = palette(ColorScheme::Dark, true);
        assert_eq!(
            hc.get(Token::DividerStroke),
            Brush::Theme(ThemeBrush::CardStroke)
        );
    }
}
