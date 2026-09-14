//! Cairo/Pango paint pipeline for the native fork.
//!
//! Cost model: a full frame only redraws on a quote change, during a short
//! glitch burst, or on window (re)allocation.  The one continuous animation —
//! the migrating scanlines — is drawn by `draw_scanline_layer` on a separate
//! transparent overlay, so it repaints only the thin scanline bands, never the
//! text/glow layer.  Per-frame heat on the main layer: one background fill,
//! one wrapped-text block (drawn as a small offscreen glyph mask for the glow,
//! then crisp), a scanline pass, and a vignette — nothing scales with pixel
//! area except the two full-screen fills.
//!
//! gtk-rs 0.18 facts honored here (learned the hard way elsewhere):
//!   - `Context::new(&ImageSurface)` returns a `Result` → `.expect()`.
//!   - `add_color_stop_rgba` takes 5 args (offset, r, g, b, a).
//!   - almost every `cr.*()` call returns `Result` → `#![allow(unused_must_use)]`.

#![allow(unused_must_use)]

use gtk::cairo;
use gtk::pango;

use crate::quotes::Quote;

/// Everything the renderer needs to know about the active look.  Built once
/// per app start from `config::Config`.
#[derive(Clone)]
pub struct Theme {
    /// Background RGB.
    pub bg: (f64, f64, f64),
    /// Foreground (quote body) RGB.
    pub fg: (f64, f64, f64),
    /// Accent cyan RGB (glow + author).
    pub cyan: (f64, f64, f64),
    /// Accent magenta RGB (glitch split).
    pub magenta: (f64, f64, f64),
    /// Accent orange RGB (author accent fallback).
    pub orange: (f64, f64, f64),
    /// Pango font family string (e.g. "JetBrains Mono").
    pub font_family: String,
    /// Body font size in CSS px.
    pub font_px: f64,
    /// Scanline darkness 0..1.
    pub scanline_alpha: f64,
    /// Scanline density: lines per screen height.
    pub scanline_lines: u32,
    /// False when the animated scanline overlay is active (draw() then skips
    /// its own static scanlines to avoid a double-darkened mesh).
    pub animated_scanlines: bool,
    /// Glitch displacement intensity 0..1.
    pub glitch_intensity: f64,
}

/// Per-draw dynamic state.
pub struct DrawState<'a> {
    /// The quote to paint.
    pub quote: &'a Quote,
    /// True while the RGB-split glitch burst is active.
    pub glitch: bool,
    /// Horizontal split offset for the glitch (px).
    pub glitch_dx: f64,
    /// Vertical split offset for the glitch (px).
    pub glitch_dy: f64,
}

/// Parse a `"#rrggbb"` hex string into an RGB tuple.
pub fn hex_color(s: &str) -> (f64, f64, f64) {
    let h = s.trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0);
    let r = ((v >> 16) & 0xff) as f64 / 255.0;
    let g = ((v >> 8) & 0xff) as f64 / 255.0;
    let b = (v & 0xff) as f64 / 255.0;
    (r, g, b)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Paint one full frame for a `w × h` monitor surface.
pub fn draw(
    cr: &cairo::Context,
    w: i32,
    h: i32,
    state: &DrawState,
    theme: &Theme,
) {
    let (fw, fh) = (w as f64, h as f64);
    let (br, bg, bb) = theme.bg;

    // 1. background
    cr.set_source_rgba(br, bg, bb, 1.0);
    cr.paint();

    // 2. glowing text block (quote + author), centered slightly above middle
    let (tx, ty) = draw_text_block(cr, fw, fh, state, theme);

    // 3. scanlines (static only when the animated overlay is not in use)
    if !theme.animated_scanlines {
        draw_scanlines(cr, fw, fh, theme, 0.0);
    }

    // 4. vignette
    draw_vignette(cr, fw, fh);

    let _ = (tx, ty);
}

// ---------------------------------------------------------------------------
// Text block
// ---------------------------------------------------------------------------

const WRAP_FRACTION: f64 = 0.86;
const BLOCK_GAP_PX: f64 = 24.0;

fn draw_text_block(
    cr: &cairo::Context,
    w: f64,
    h: f64,
    state: &DrawState,
    theme: &Theme,
) -> (f64, f64) {
    let wrap = (w * WRAP_FRACTION).max(120.0);

    let quote_layout = make_layout(cr, theme, &state.quote.text, wrap);
    let author_layout = make_layout(cr, theme, &format!("— {}", state.quote.author), wrap);

    // Center on what is actually *painted* (ink), not on Pango's logical
    // width: `pixel_size` can return a box whose ink starts/overhangs
    // asymmetrically (observed +43 px right-shift inside the logical box),
    // which pushes the visible text off center.
    let (qink, _qlog) = quote_layout.pixel_extents();
    let (aink, _alog) = author_layout.pixel_extents();
    let (iw, ih) = (qink.width() as f64, qink.height() as f64);
    let (aw, ah) = (aink.width() as f64, aink.height() as f64);

    let total = ih + BLOCK_GAP_PX + ah;
    let top = (h - total) / 2.0 - h * 0.04;

    let qx = w / 2.0 - qink.x() as f64 - iw / 2.0;
    let qy = top - qink.y() as f64;
    let ax = w / 2.0 - aink.x() as f64 - aw / 2.0;
    let ay = top + ih + BLOCK_GAP_PX - aink.y() as f64;

    // glow behind the quote + author
    draw_glow(cr, &quote_layout, qx, qy, theme.cyan, 0.30);
    draw_glow(cr, &author_layout, ax, ay, theme.cyan, 0.18);

    if state.glitch {
        // RGB-split: magenta left, cyan right, white center.
        let s = (theme.glitch_intensity * 18.0).max(2.0);
        let dx = state.glitch_dx * s;
        let dy = state.glitch_dy * s;
        draw_text(cr, &quote_layout, qx + dx, qy, theme.magenta, 0.9);
        draw_text(cr, &quote_layout, qx - dx, qy + dy, theme.cyan, 0.9);
    }
    draw_text(cr, &quote_layout, qx, qy, theme.fg, 1.0);
    draw_terminal_cursor(cr, &quote_layout, qx, qy, theme);

    let (ar, ag, ab) = theme.cyan;
    draw_text_fade_author(cr, &author_layout, ax, ay, theme, (ar, ag, ab));
    (qx, qy)
}

/// Build a wrapped, centered Pango layout from a context.
fn make_layout(cr: &cairo::Context, theme: &Theme, text: &str, wrap_px: f64) -> pango::Layout {
    let layout = pangocairo::create_layout(cr);
    let mut desc = pango::FontDescription::from_string(&theme.font_family);
    // CSS px → Pango size (Pango units are points * SCALE; 96 dpi ⇒ px * 0.75 pt)
    let points = theme.font_px * 0.75;
    desc.set_size((points * pango::SCALE as f64) as i32);
    layout.set_font_description(Some(&desc));
    layout.set_width((wrap_px * pango::SCALE as f64) as i32);
    layout.set_alignment(pango::Alignment::Center);
    layout.set_wrap(pango::WrapMode::WordChar);
    layout.set_text(text);
    layout
}

/// Draw one crisp text pass at `x,y` in `color` with optional alpha.
fn draw_text(cr: &cairo::Context, layout: &pango::Layout, x: f64, y: f64, color: (f64, f64, f64), alpha: f64) {
    let (r, g, b) = color;
    cr.set_source_rgba(r, g, b, alpha);
    cr.translate(x, y);
    pangocairo::show_layout(cr, layout);
    cr.translate(-x, -y);
}

/// Author line: smaller, cyan with a soft secondary orange layer beneath.
fn draw_text_fade_author(
    cr: &cairo::Context,
    layout: &pango::Layout,
    x: f64,
    y: f64,
    theme: &Theme,
    color: (f64, f64, f64),
) {
    let (or_, og, ob) = theme.orange;
    draw_text(cr, layout, x + 1.2, y + 1.2, (or_, og, ob), 0.55);
    draw_text(cr, layout, x, y, color, 1.0);
}

/// Build the glow as an offscreen glyph mask composited via `mask_surface`
/// several times at growing radius (a poor man's box blur).
///
/// The mask must be sized from the layout's *ink* extents, not the logical
/// box (`pixel_size`): a centered layout can have its ink start inside and
/// overhang past the logical box by tens of px, and any ink outside the mask
/// surface gets no glow — the trailing words of a long quote then render with
/// a hard edge instead of the halo.
fn draw_glow(
    cr: &cairo::Context,
    layout: &pango::Layout,
    x: f64,
    y: f64,
    color: (f64, f64, f64),
    base_alpha: f64,
) {
    let (ink, _logical) = layout.pixel_extents();
    let iw = ink.width() as f64;
    let ih = ink.height() as f64;
    let (ix, iy) = (ink.x() as f64, ink.y() as f64);
    if iw <= 0.0 || ih <= 0.0 {
        return;
    }
    let pad = 24.0;
    let sw = iw + 2.0 * pad;
    let sh = ih + 2.0 * pad;

    let surf = cairo::ImageSurface::create(cairo::Format::ARgb32, sw as i32, sh as i32)
        .expect("glow glyph surface");
    {
        let c2 = cairo::Context::new(&surf).expect("glow glyph context");
        c2.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        // Shift so the INK rect (not the logical box) lands inside the pad.
        c2.translate(pad - ix, pad - iy);
        pangocairo::show_layout(&c2, layout);
    }

    // Screen position of the ink's top-left corner.
    let ox = x + ix;
    let oy = y + iy;

    let (r, g, b) = color;
    for i in 1..=8 {
        let radius = i as f64 * 2.0;
        let a = base_alpha * (0.5 - (i as f64 - 1.0) / 12.0);
        if a <= 0.0 {
            break;
        }
        cr.set_source_rgba(r, g, b, a);
        cr.mask_surface(&surf, ox - pad + radius, oy - pad);
        cr.mask_surface(&surf, ox - pad - radius, oy - pad);
        cr.mask_surface(&surf, ox - pad, oy - pad + radius);
        cr.mask_surface(&surf, ox - pad, oy - pad - radius);
    }
}

/// Terminal-style block cursor right after the last character of the quote.
/// Uses `Layout::index_to_pos` to find the caret point.
fn draw_terminal_cursor(
    cr: &cairo::Context,
    layout: &pango::Layout,
    x: f64,
    y: f64,
    theme: &Theme,
) {
    let text_len = layout.text().len() as i32;
    let rect = layout.index_to_pos(text_len); // Pango units (1/1024 px)
    let sc = pango::SCALE as f64;
    let cx = x + rect.x() as f64 / sc;
    let cy = y + rect.y() as f64 / sc;
    let cw = (rect.width() as f64 / sc).clamp(2.0, theme.font_px * 0.7);
    let ch = (rect.height() as f64 / sc).max(2.0);

    let (crr, cgg, cbb) = theme.cyan;
    cr.set_source_rgba(crr, cgg, cbb, 0.9);
    cr.rectangle(cx, cy, cw, ch);
    cr.fill();
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

/// Paint only the moving scanlines, for the transparent overlay layer on top
/// of the static text frame.  `phase` (px, 0..spacing) rolls the whole mesh
/// from the top edge toward the bottom; it wraps seamlessly every `spacing`
/// because the band pattern repeats with that period.
pub fn draw_scanline_layer(cr: &cairo::Context, w: i32, h: i32, theme: &Theme, phase: f64) {
    draw_scanlines(cr, w as f64, h as f64, theme, phase);
}

/// Horizontal CRT scanlines: dark bands every `spacing = H / lines` pixels,
/// offset by `phase` so the mesh can crawl downward.
fn draw_scanlines(cr: &cairo::Context, w: f64, h: f64, theme: &Theme, phase: f64) {
    if theme.scanline_alpha <= 0.0 || theme.scanline_lines == 0 {
        return;
    }
    let spacing = (h / theme.scanline_lines as f64).max(2.0);
    let thickness = (spacing * 0.35).clamp(1.0, 3.0);

    // First band sits just above the top edge and scrolls into view; the
    // modulo keeps the wrap at exactly one period (no seam at the edge).
    let start = (phase % spacing) - spacing + thickness / 2.0;

    cr.set_source_rgba(0.0, 0.0, 0.0, theme.scanline_alpha);
    let mut y = start;
    while y < h {
        cr.rectangle(0.0, y - thickness / 2.0, w, thickness);
        cr.fill();
        y += spacing;
    }
}

/// Radial darkening toward the screen edges for the CRT look.
fn draw_vignette(cr: &cairo::Context, w: f64, h: f64) {
    let cx = w / 2.0;
    let cy = h / 2.0;
    let radius = (w * w + h * h).sqrt() * 0.58;

    let grad = cairo::RadialGradient::new(cx, cy, 0.0, cx, cy, radius);
    grad.add_color_stop_rgba(0.00, 0.0, 0.0, 0.0, 0.0);
    grad.add_color_stop_rgba(0.72, 0.0, 0.0, 0.0, 0.0);
    grad.add_color_stop_rgba(1.00, 0.0, 0.0, 0.0, 0.62);

    cr.set_source(&grad);
    cr.paint();
}

// ---------------------------------------------------------------------------
// Tests — render headlessly to a PNG so the output can be eyeballed without
// touching the user's live desktop.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_theme() -> Theme {
        Theme {
            bg: (0.02, 0.03, 0.05),
            fg: (1.0, 1.0, 1.0),
            cyan: (0.22, 0.90, 1.0),
            magenta: (1.0, 0.24, 0.87),
            orange: (1.0, 0.89, 0.70),
            font_family: "monospace".into(),
            font_px: 24.0,
            scanline_alpha: 0.3,
            scanline_lines: 100,
            animated_scanlines: false,
            glitch_intensity: 0.35,
        }
    }

    fn test_quote() -> Quote {
        Quote {
            text: "The future belongs to those who show up before it arrives.".into(),
            author: "Mira Voss".into(),
        }
    }

/// Render one frame to an offscreen surface and dump it to a PNG.
    #[test]
    fn render_writes_text_output() {
        let (w, h) = (900, 400);
        let surf = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
        let cr = cairo::Context::new(&surf).expect("context");

        let theme = test_theme();
        let state = DrawState {
            quote: &test_quote(),
            glitch: true,
            glitch_dx: 0.6,
            glitch_dy: 0.3,
        };
        draw(&cr, w, h, &state, &theme);

        let mut file = std::fs::File::create("/tmp/render_test.png").expect("create png");
        surf.as_ref().write_to_png(&mut file).expect("png");
    }

    /// Regression: the glow mask used to be sized/placed from the LOGICAL box,
    /// which for wrapped CENTERED text sits offset inside Pango (`logical.x`>0)
    /// and right of the origin, so the tail of the last line poked out of the
    /// mask surface and the trailing words ("he lesson") lost their halo.
    #[test]
    fn render_glow_covers_right_edge_of_long_quote() {
        let (w, h) = (2560, 1440);
        let mut surf = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");

        let theme = Theme {
            font_px: 37.0,
            ..test_theme()
        };
        let quote = Quote {
            text: "Resistance is not always the enemy - sometimes it is the shape of the lesson. Resistance is not always the enemy - sometimes it is the shape of the lesson. Resistance is not always the enemy - sometimes it is the shape of the lesson.".into(),
            author: "R. Lesson".into(),
        };
        let state = DrawState {
            quote: &quote,
            glitch: false,
            glitch_dx: 0.0,
            glitch_dy: 0.0,
        };
        let (tx, ty, iw, ih, ix, iy) = {
            let cr = cairo::Context::new(&surf).expect("context");
            let (tx, ty) = draw_text_block(&cr, w as f64, h as f64, &state, &theme);
            let layout = make_layout(&cr, &theme, &state.quote.text, w as f64 * WRAP_FRACTION);
            let (ink, _log) = layout.pixel_extents();
            (
                tx,
                ty,
                ink.width() as f64,
                ink.height() as f64,
                ink.x() as f64,
                ink.y() as f64,
            )
        }; // drop cr so surf.data() gets exclusive access

        // The failure was razor-specific: with the logical-box mask, the LONGEST
        // line's ink tail poked ~28px out of the mask so its final words lost
        // the halo.  Sample the extreme rightmost columns of the ink rect —
        // with the bug they contain ~zero glow (mask cut off at ink_right-28).
        let x0 = (tx + ix + iw - 8.0) as usize;
        let x1 = (tx + ix + iw) as usize;
        let y0 = (ty + iy) as usize;
        let y1 = (ty + iy + ih) as usize;

        let mut glow = 0i64;
        let data = surf.data().expect("surface data");
        for y in y0..y1 {
            for x in x0..x1 {
                let pl = (y * (w as usize) + x) * 4;
                // Premultiplied BGRA little-endian: [B, G, R, A].
                let blue = data[pl];
                let red = data[pl + 2];
                if blue as i16 - red as i16 >= 14 {
                    glow += 1;
                }
            }
        }
        assert!(
            glow > 100,
            "expected cyan glow at the extreme right edge of the wrapped quote, got {glow} cyan px"
        );
    }

    /// The crisp text pass must actually change pixels in the center band.
    #[test]
    fn render_is_not_blank() {
        let (w, h) = (900, 400);
        let mut surf = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");

        let theme = test_theme();
        let state = DrawState {
            quote: &test_quote(),
            glitch: false,
            glitch_dx: 0.0,
            glitch_dy: 0.0,
        };
        {
            let cr = cairo::Context::new(&surf).expect("context");
            draw(&cr, w, h, &state, &theme);
        } // drop the context so surf.data() can get exclusive access

        // Sample the center band (text area, before vignette darkens the edges).
        let data = surf.data().expect("surface data");
        let mut bright = 0i64;
        for y in (h / 2 - 30)..(h / 2 + 30) {
            for x in 0..w {
                let pl = (y * w as i32 + x) as usize * 4;
                // Premultiplied BGRA on little-endian: bytes [B,G,R,A].
                let red = data[pl + 2];
                if red > 0x50 {
                    bright += 1;
                }
            }
        }
        assert!(
            bright > 50,
            "expected at least 50 bright pixels in the text band, got {bright}"
        );
    }
}