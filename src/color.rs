//! Colour type for themes and icon rendering.
//!
//! Replaces `egui::Color32`. The UI is native Win32, so the only consumers are
//! the theme parser, the procedural icon renderer and the panel painter; none
//! of them needs a full colour library. Straight 8-bit RGBA, non-premultiplied,
//! which is what both `CreateDIBSection` and the icon renderer expect.

/// Non-premultiplied 8-bit RGBA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Color {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
    pub(crate) a: u8,
}

impl Color {
    pub(crate) const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    #[must_use]
    pub(crate) const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    #[must_use]
    pub(crate) const fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorref_is_bgr_not_rgb() {
        // Pure red must land in the low byte, not the high one.
        assert_eq!(Color::from_rgb(0xFF, 0x00, 0x00).to_colorref(), 0x0000_00FF);
        assert_eq!(Color::from_rgb(0x00, 0x00, 0xFF).to_colorref(), 0x00FF_0000);
    }

    #[test]
    fn alpha_blend_endpoints() {
        let fg = Color::from_rgba(255, 0, 0, 0);
        let bg = Color::from_rgb(0, 0, 255);
        assert_eq!(fg.over(bg), bg, "zero alpha keeps the background");

        let solid = Color::from_rgba(255, 0, 0, 255);
        assert_eq!(solid.over(bg), Color::from_rgb(255, 0, 0));
    }

    #[test]
    fn shift_lightens_and_darkens_uniformly() {
        let grey = Color::from_rgb(0x25, 0x25, 0x26);
        // Positive lightens every channel by the same step.
        assert_eq!(grey.shifted(0x14), Color::from_rgb(0x39, 0x39, 0x3A));
        // Negative darkens by the same step.
        assert_eq!(grey.shifted(-0x14), Color::from_rgb(0x11, 0x11, 0x12));
        // Zero is identity.
        assert_eq!(grey.shifted(0), grey);
    }

    #[test]
    fn shift_clamps_at_both_ends() {
        // Near-white lightened does not wrap past 255.
        assert_eq!(
            Color::from_rgb(0xF8, 0xFA, 0xFF).shifted(0x30),
            Color::from_rgb(0xFF, 0xFF, 0xFF)
        );
        // Near-black darkened does not underflow below 0.
        assert_eq!(
            Color::from_rgb(0x08, 0x04, 0x00).shifted(-0x30),
            Color::from_rgb(0x00, 0x00, 0x00)
        );
    }

    #[test]
    fn shift_preserves_alpha() {
        let c = Color::from_rgba(0x40, 0x40, 0x40, 0x80);
        assert_eq!(c.shifted(0x10).a, 0x80);
    }
}
