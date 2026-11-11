//! Colors: the same calm dark palette as Files and Disk Usage.

use gpui::{Hsla, hsla, rgb};

pub fn bg() -> Hsla {
    rgb(0x0c0e13).into()
}
pub fn panel() -> Hsla {
    rgb(0x12151c).into()
}
pub fn raised() -> Hsla {
    rgb(0x181c25).into()
}
pub fn hover() -> Hsla {
    hsla(0.0, 0.0, 1.0, 0.045)
}
pub fn border() -> Hsla {
    rgb(0x222733).into()
}
pub fn border_strong() -> Hsla {
    rgb(0x2e3443).into()
}
pub fn text() -> Hsla {
    rgb(0xe6e9ef).into()
}
pub fn text_soft() -> Hsla {
    rgb(0xc4cad6).into()
}
pub fn muted() -> Hsla {
    rgb(0x8b93a5).into()
}
pub fn faint() -> Hsla {
    rgb(0x5a6276).into()
}
pub fn accent() -> Hsla {
    rgb(0x7c9cff).into()
}
pub fn accent_soft() -> Hsla {
    hsla(226.0 / 360.0, 1.0, 0.74, 0.16)
}
pub fn accent_strong() -> Hsla {
    hsla(226.0 / 360.0, 1.0, 0.74, 0.26)
}
pub fn on_accent() -> Hsla {
    rgb(0x0b1020).into()
}
pub fn danger() -> Hsla {
    rgb(0xff7a85).into()
}
pub fn success() -> Hsla {
    rgb(0x7ddba8).into()
}
pub fn warning() -> Hsla {
    rgb(0xf2c46d).into()
}
pub fn card() -> Hsla {
    hsla(225.0 / 360.0, 0.2, 0.08, 0.96)
}

// ── Grid ────────────────────────────────────────────────────────────────

pub fn header_bg() -> Hsla {
    rgb(0x11141b).into()
}
pub fn zebra() -> Hsla {
    hsla(0.0, 0.0, 1.0, 0.018)
}
pub fn selection() -> Hsla {
    hsla(226.0 / 360.0, 1.0, 0.74, 0.13)
}
pub fn grid_line() -> Hsla {
    rgb(0x1a1e28).into()
}
pub fn grid_line_strong() -> Hsla {
    rgb(0x262b38).into()
}
pub fn scrollbar() -> Hsla {
    hsla(0.0, 0.0, 1.0, 0.16)
}
