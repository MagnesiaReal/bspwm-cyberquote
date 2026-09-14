//! Cairo/Pango paint pipeline for the native fork.
//!
//! Cost model: a full frame only redraws on a quote change, during the one-shot
//! typewriter reveal (~30 ms/char until the phrase is fully shown), during a
//! short glitch burst, or on window (re)allocation.  The one continuous
//! animation — the migrating scanlines — is drawn by `draw_scanline_tile` on a
//! separate transparent overlay: the mesh is pre-rendered once into an A8 tile
//! one period tall, and each frame is a single `Extend::Repeat` pattern paint (a
//! cached-pixmap blit), never the text/glow layer.  When that overlay exists it
//! ALSO paints the blinking terminal caret (via `cursor_rect`, recolored to
//! `accent.author_color`) so its ~2 Hz blink costs one small rect fill per
//! frame — the main layer draws the caret itself only in the no-compositor
//! fallback, gated by `DrawState.cursor_on`.  Per-frame heat on the main layer:
//! one background fill, one wrapped-text block, a scanline pass, and a vignette
//! — nothing scales with pixel area except the two full-screen fills.
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
    /// Accent cyan RGB (author ink + glitch split).
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
    /// Text glow: blur radius in px (0 disables the glow layer).
    pub glow_radius: f64,
    /// Text glow opacity 0..1 applied to the blurred secondary layer.
    pub glow_alpha: f64,
    /// Text glow horizontal shift in px (organic offset vs. crisp text).
    pub glow_offset_x: f64,
    /// Text glow vertical shift in px (organic offset vs. crisp text).
    pub glow_offset_y: f64,
    /// Text glow tint RGB (defaults to the body color = white).
    pub glow_color: (f64, f64, f64),
    /// Author ink + terminal caret RGB (the `accent.author_color` knob).
    pub author_ink: (f64, f64, f64),
}

/// Per-draw dynamic state.
pub struct DrawState<'a> {
    /// The quote to paint.
    pub quote: &'a Quote,
    /// True while the glitch burst is active.
    pub glitch: bool,
    /// Whole-block horizontal displacement for this step (px).
    pub glitch_dx: f64,
    /// Whole-block vertical displacement for this step (px).
    pub glitch_dy: f64,
    /// Chromatic aberration: cyan/magenta echo layers at ±this offset (px);
    /// 0 disables the echoes for this step.
    pub glitch_echo_px: f64,
    /// Negative "invert" flash: bright bar behind the block, dark ink.
    pub glitch_invert: bool,
    /// Strobe: skip the crisp text (and echoes) — a brief blank frame.
    pub glitch_ghost: bool,
    /// Typewriter reveal: number of quote characters already shown.
    ///
    /// The quote body is clipped line-by-line to the caret of this many chars
    /// (one-shot per phrase). When it equals the text length the block is
    /// fully revealed (glow + author + end cursor) and rendering is `static`.
    pub typewriter: usize,
    /// Cursor visibility for the blink phase (`false` = the block caret is
    /// hidden this tick). The main layer honors it only when it owns the
    /// cursor (`theme.animated_scanlines`, i.e. no RGBA overlay); with a
    /// compositor the caret is painted on the scanline overlay instead.
    pub cursor_on: bool,
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

    let total_chars = state.quote.text.chars().count();
    let typed = state.typewriter.min(total_chars);
    let typing = typed < total_chars;
    if state.glitch {
        // Full-text glitch burst (mirrors the HTML `glitch-full` keyframes):
        // the whole block swings left/right with cyan/magenta echo copies,
        // occasional white invert flashes and strobe blank frames.
        let ox = qx + state.glitch_dx;
        let oy = qy + state.glitch_dy;
        if !state.glitch_ghost {
            let echo = state.glitch_echo_px;
            if echo > 0.5 {
                draw_text(cr, &quote_layout, ox + echo, oy, theme.magenta, 0.85);
                draw_text(cr, &quote_layout, ox - echo, oy, theme.cyan, 0.85);
            }
            if state.glitch_invert {
                let pad2 = 16.0;
                let (ink, _) = quote_layout.pixel_extents();
                let (fr, fg2, fb) = theme.fg;
                let (bgr, bgg, bgb) = theme.bg;
                cr.set_source_rgba(fr, fg2, fb, 0.88);
                cr.rectangle(
                    ox + ink.x() as f64 - pad2,
                    oy + ink.y() as f64 - pad2,
                    ink.width() as f64 + 2.0 * pad2,
                    ink.height() as f64 + 2.0 * pad2,
                );
                cr.fill();
                draw_text(cr, &quote_layout, ox, oy, (bgr, bgg, bgb), 1.0);
            } else {
                draw_text(cr, &quote_layout, ox, oy, theme.fg, 0.95);
            }
        }
    } else if typing {
        // One-shot typewriter reveal: clip the layout line-by-line to the
        // caret of the next character so lines stay centered and stable
        // while they appear left→right.
        let byte = char_byte_index(&state.quote.text, typed);
        cr.save();
        push_typewriter_clip(cr, &quote_layout, byte, qx, qy);
        draw_text(cr, &quote_layout, qx, qy, theme.fg, 1.0);
        cr.restore();
        // The main layer owns the caret only in the no-overlay fallback
        // (`animated_scanlines` set); with a compositor the scanline overlay
        // paints it instead.
        if theme.animated_scanlines && state.cursor_on {
            draw_terminal_cursor(cr, &quote_layout, qx, qy, byte, theme);
        }
    } else {
        draw_text_with_glow(cr, &quote_layout, qx, qy, theme);
        if theme.animated_scanlines && state.cursor_on {
            draw_terminal_cursor(cr, &quote_layout, qx, qy, quote_layout.text().len(), theme);
        }
    }

    // The author line joins the animation only once the quote body is fully
    // revealed. A glitch ghost strobe blanks the whole block, author included.
    if !typing && !(state.glitch && state.glitch_ghost) {
        let (ar, ag, ab) = theme.author_ink;
        draw_text_fade_author(cr, &author_layout, ax, ay, theme, (ar, ag, ab));
    }
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

/// Draw `layout` at `x,y` with a blurred secondary pass behind it.
///
/// Layer order (per the glow spec, no external deps):
///   1. the SAME text is rasterized into an offscreen ARGB surface sized from
///      the layout's ink extents (plus blur padding), so nothing overflows;
///   2. that layer is low-pass blurred with a pure-Cairo down-sample /
///      bilinear up-sample, then composited BEHIND the text via `mask_surface`
///      — `glow_color` tints it, `glow_alpha` dims it, and `glow_offset_x/y`
///      shift it for an organic feel;
///   3. the original crisp pass is drawn on top unchanged.
///
/// The crisp pass is byte-for-byte the same draw as without a glow, so text
/// sharpness is never degraded. Radius ≤ 0 or alpha ≤ 0 falls back to a plain
/// crisp draw.
fn draw_text_with_glow(
    cr: &cairo::Context,
    layout: &pango::Layout,
    x: f64,
    y: f64,
    theme: &Theme,
) {
    let (gr, gg, gb) = theme.glow_color;
    if theme.glow_radius <= 0.0 || theme.glow_alpha <= 0.0 {
        draw_text(cr, layout, x, y, theme.fg, 1.0);
        return;
    }

    // 1. Rasterize the same glyphs into an offscreen layer with blur padding.
    let (ink, _log) = layout.pixel_extents();
    let (iw, ih) = (ink.width() as f64, ink.height() as f64);
    if iw <= 0.0 || ih <= 0.0 {
        draw_text(cr, layout, x, y, theme.fg, 1.0);
        return;
    }
    let pad = theme.glow_radius + 4.0;
    let sw = (iw + 2.0 * pad).ceil().max(1.0) as i32;
    let sh = (ih + 2.0 * pad).ceil().max(1.0) as i32;
    let layer = cairo::ImageSurface::create(cairo::Format::ARgb32, sw, sh)
        .expect("glow layer surface");
    {
        let lc = cairo::Context::new(&layer).expect("glow layer context");
        lc.set_source_rgba(0.0, 0.0, 0.0, 0.0);
        lc.paint().ok();
        lc.translate(pad - ink.x() as f64, pad - ink.y() as f64);
        lc.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        pangocairo::show_layout(&lc, layout);
    }

    // 2. Blur the layer (radius ≥ 1 px) with a down/up bilinear low-pass.
    let blurred = if theme.glow_radius >= 1.0 {
        blur_downup(&layer, theme.glow_radius)
    } else {
        layer
    };

    // 3. Composite the glow behind the crisp text, ink-aligned with offsets.
    cr.set_source_rgba(gr, gg, gb, theme.glow_alpha.clamp(0.0, 1.0));
    cr.mask_surface(
        &blurred,
        x + ink.x() as f64 - pad + theme.glow_offset_x,
        y + ink.y() as f64 - pad + theme.glow_offset_y,
    );

    // 4. The crisp original on top, untouched.
    draw_text(cr, layout, x, y, theme.fg, 1.0);
}

/// Cheap progressively-soft blur without external blurs: down-sample the layer
/// to ~`radius/2` then bilinear up-sample back. Each stage is a single paint +
/// bilinear-filtered pattern, so the falloff is smooth instead of a hard halo.
fn blur_downup(src: &cairo::ImageSurface, radius: f64) -> cairo::ImageSurface {
    let (sw, sh) = (src.width() as f64, src.height() as f64);
    let sc = (radius * 0.5).clamp(1.5, 10.0);
    let dw = (sw / sc).ceil().max(1.0) as i32;
    let dh = (sh / sc).ceil().max(1.0) as i32;

    let small = cairo::ImageSurface::create(cairo::Format::ARgb32, dw, dh)
        .expect("blur down surface");
    {
        let c = cairo::Context::new(&small).expect("blur down context");
        c.scale(dw as f64 / sw, dh as f64 / sh);
        let pat = cairo::SurfacePattern::create(src);
        pat.set_filter(cairo::Filter::Bilinear);
        c.set_source(&pat);
        c.paint().ok();
    }

    let out =
        cairo::ImageSurface::create(cairo::Format::ARgb32, sw as i32, sh as i32)
            .expect("blur up surface");
    {
        let c = cairo::Context::new(&out).expect("blur up context");
        c.scale(sw / dw as f64, sh / dh as f64);
        let pat = cairo::SurfacePattern::create(&small);
        pat.set_filter(cairo::Filter::Bilinear);
        c.set_source(&pat);
        c.paint().ok();
    }
    out
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

/// Terminal-style block cursor at byte offset `at` of the quote (the insert
/// position right after the last revealed character). Uses `Layout::index_to_pos`.
fn draw_terminal_cursor(
    cr: &cairo::Context,
    layout: &pango::Layout,
    x: f64,
    y: f64,
    at: usize,
    theme: &Theme,
) {
    let rect = layout.index_to_pos(at as i32); // Pango units (1/1024 px)
    let sc = pango::SCALE as f64;
    let cx = x + rect.x() as f64 / sc;
    let cy = y + rect.y() as f64 / sc;
    let cw = (rect.width() as f64 / sc).clamp(2.0, theme.font_px * 0.7);
    let ch = (rect.height() as f64 / sc).max(2.0);

    let (crr, cgg, cbb) = theme.author_ink;
    cr.set_source_rgba(crr, cgg, cbb, 0.9);
    cr.rectangle(cx, cy, cw, ch);
    cr.fill();
}

/// Number of quote chars first shown during the one-shot typewriter reveal.
/// Converts a count of Unicode scalar values to the Pango byte-index caret.
fn char_byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

/// Device-space rect of the terminal caret for `text` (`typed` chars shown) on
/// a `w × h` frame.  Mirrors `draw_text_block`'s layout/origin math so the
/// animated scanline overlay paints the caret dead on the main layer's crisp
/// text (same wrap, centering, and author-line offset).  A fully-typed phrase
/// places the caret after the last character (the standalone "end cursor").
/// Returns `(x, y, w_caret, h_caret)`; `None` only for a pathological layout.
pub fn cursor_rect(
    cr: &cairo::Context,
    theme: &Theme,
    text: &str,
    author: &str,
    typed: usize,
    w: f64,
    h: f64,
) -> Option<(f64, f64, f64, f64)> {
    let wrap = (w * WRAP_FRACTION).max(120.0);
    let quote_layout = make_layout(cr, theme, text, wrap);
    let author_layout = make_layout(cr, theme, &format!("— {author}"), wrap);

    let (qink, _) = quote_layout.pixel_extents();
    let (aink, _) = author_layout.pixel_extents();
    let iw = qink.width() as f64;
    let ih = qink.height() as f64;
    let ah = aink.height() as f64;

    let top = (h - (ih + BLOCK_GAP_PX + ah)) / 2.0 - h * 0.04;
    let qx = w / 2.0 - qink.x() as f64 - iw / 2.0;
    let qy = top - qink.y() as f64;

    let total = text.chars().count();
    let byte = if typed >= total {
        quote_layout.text().len()
    } else {
        char_byte_index(text, typed)
    };

    let rect = quote_layout.index_to_pos(byte as i32);
    let sc = pango::SCALE as f64;
    Some((
        qx + rect.x() as f64 / sc,
        qy + rect.y() as f64 / sc,
        (rect.width() as f64 / sc).clamp(2.0, theme.font_px * 0.7),
        (rect.height() as f64 / sc).max(2.0),
    ))
}

/// Clip the context so the (possibly centered, wrapped) layout reveals only
/// its first `byte` bytes: completed lines are fully visible, the caret line
/// only up to the insertion point.
fn push_typewriter_clip(
    cr: &cairo::Context,
    layout: &pango::Layout,
    byte: usize,
    x: f64,
    y: f64,
) {
    let sc = pango::SCALE as f64;
    let line_count = layout.line_count();
    let (caret_line, x_pos) = layout.index_to_line_x(byte as i32, false);
    let caret_px = x_pos as f64 / sc;

    for i in 0..line_count {
        let Some(line) = layout.line(i) else { continue };
        let (_, logical) = line.extents();
        // `index_to_pos` at the line's start gives the caret row in LAYOUT
        // coordinates (the alignment/centering offset included), so `col` is
        // the line's true left edge and (top, height) its row band. The line's
        // logical rect alone is line-relative and misses the centering.
        let row = layout.index_to_pos(line.start_index() as i32);
        let col = row.x() as f64 / sc;
        let top = row.y() as f64 / sc;
        let row_h = row.height() as f64 / sc;
        if i < caret_line {
            let lw = logical.width() as f64 / sc;
            cr.rectangle(x + col, y + top, lw, row_h);
        } else if i == caret_line {
            // `index_to_line_x` x_pos is measured from the caret line's own
            // start (not the layout origin); add 2 px so antialiased glyph
            // edges breathe at the caret line.
            let lw = (caret_px + 2.0).max(0.0);
            cr.rectangle(x + col, y + top, lw, row_h);
        }
    }
    cr.clip();
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

/// Minimum height of one repeating scanline tile (integer, so the pattern
/// period matches the timer's phase wrap exactly and no seam can accumulate).
pub fn scanline_tile_height(h: f64, lines: u32) -> f64 {
    ((h / lines.max(1) as f64).max(2.0)).ceil()
}

/// Pre-render one tile of the scanline mesh: `w × period` px, an A8 alpha
/// mask holding a single dark band at its top.  The overlay layer paints this
/// tile with `Extend::Repeat`, so animating is ONE composited blit per frame
/// instead of clearing the whole surface and issuing hundreds of rect fills.
/// The tile is exactly one period tall, so the line+gap seam stays exact.
pub fn scanline_tile(theme: &Theme, w: i32, h: i32) -> Option<cairo::ImageSurface> {
    if theme.scanline_alpha <= 0.0 || theme.scanline_lines == 0 || w <= 0 || h <= 0 {
        return None;
    }
    let spacing = (h as f64 / theme.scanline_lines as f64).max(2.0);
    let period = spacing.ceil();
    let thickness = ((spacing * 0.35).clamp(1.0, 3.0) - 1.0).max(1.0);

    let surf = cairo::ImageSurface::create(cairo::Format::A8, w, period as i32).ok()?;
    let tc = cairo::Context::new(&surf).ok()?;
    tc.set_source_rgba(0.0, 0.0, 0.0, theme.scanline_alpha);
    tc.rectangle(0.0, 0.0, w as f64, thickness);
    tc.fill();
    Some(surf)
}

/// Paint the pre-rendered tile across the whole `w × h` layer at `phase`:
/// one repeating-pattern composite (no explicit clear, no per-band rects).
/// The pattern matrix shifts the mesh by `phase` px, so it scrolls top→bottom
/// and wraps seamlessly exactly one period later.
pub fn draw_scanline_tile(
    cr: &cairo::Context,
    theme: &Theme,
    phase: f64,
    tile: &cairo::ImageSurface,
) {
    if theme.scanline_alpha <= 0.0 || tile.width() <= 0 || tile.height() <= 0 {
        return;
    }
    let pat = cairo::SurfacePattern::create(tile);
    pat.set_extend(cairo::Extend::Repeat);
    pat.set_matrix(cairo::Matrix::new(1.0, 0.0, 0.0, 1.0, 0.0, -phase));
    cr.set_source(&pat);
    cr.paint();
}

/// Horizontal CRT scanlines: dark bands every `spacing = H / lines` pixels,
/// offset by `phase` so the mesh can crawl downward.
fn draw_scanlines(cr: &cairo::Context, w: f64, h: f64, theme: &Theme, phase: f64) {
    if theme.scanline_alpha <= 0.0 || theme.scanline_lines == 0 {
        return;
    }
    let spacing = (h / theme.scanline_lines as f64).max(2.0);
    // 1 px thinner than the natural 35% of the pitch, never below a hairline.
    let thickness = ((spacing * 0.35).clamp(1.0, 3.0) - 1.0).max(1.0);

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
            glow_radius: 6.0,
            glow_alpha: 0.30,
            glow_offset_x: 1.5,
            glow_offset_y: 2.0,
            glow_color: (1.0, 1.0, 1.0),
            author_ink: (0.63, 0.16, 0.90),
        }
    }

    fn test_quote() -> Quote {
        Quote {
            text: "The future belongs to those who show up before it arrives.".into(),
            author: "Mira Voss".into(),
        }
    }

    /// Count faint-bright pixels (the soft halo zone) in the center band.
    /// Red channel strictly between the bg floor and the crisp-glyph ceiling.
    fn count_halo(surf: &mut cairo::ImageSurface, w: i32, h: i32) -> i64 {
        let data = surf.data().expect("surface data");
        let mut n = 0i64;
        for y in (h / 2 - 60)..(h / 2 + 60) {
            for x in 0..w {
                let pl = (y * w as i32 + x) as usize * 4;
                let red = data[pl + 2];
                if red > 0x12 && red <= 0x9F {
                    n += 1;
                }
            }
        }
        n
    }

    /// The offscreen glow layer must soften the glyph edges: with a glow, the
    /// frame has strictly more mid-brightness pixels than the identical render
    /// with the glow disabled (the halo is the blurred secondary layer).
    #[test]
    fn glow_adds_soft_halo() {
        let (w, h) = (900, 400);
        let mut glow_theme = test_theme();
        glow_theme.glow_radius = 8.0;
        glow_theme.glow_alpha = 0.55;
        glow_theme.glow_offset_x = 0.0;
        glow_theme.glow_offset_y = 0.0;
        let mut plain_theme = test_theme();
        plain_theme.glow_radius = 0.0;
        plain_theme.glow_alpha = 0.0;

        let mut plain = {
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
            {
                let cr = cairo::Context::new(&s).expect("context");
                let state = DrawState {
                    quote: &test_quote(),
                    glitch: false,
                    glitch_dx: 0.0,
                    glitch_dy: 0.0,
                    glitch_echo_px: 0.0,
                    glitch_invert: false,
                    glitch_ghost: false,
                    typewriter: test_quote().text.chars().count(),
                    cursor_on: true,
                };
                draw(&cr, w, h, &state, &plain_theme);
            }
            s
        };
        let mut glow = {
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
            {
                let cr = cairo::Context::new(&s).expect("context");
                let state = DrawState {
                    quote: &test_quote(),
                    glitch: false,
                    glitch_dx: 0.0,
                    glitch_dy: 0.0,
                    glitch_echo_px: 0.0,
                    glitch_invert: false,
                    glitch_ghost: false,
                    typewriter: test_quote().text.chars().count(),
                    cursor_on: true,
                };
                draw(&cr, w, h, &state, &glow_theme);
            }
            s
        };

        let plain_halo = count_halo(&mut plain, w, h);
        let glow_halo = count_halo(&mut glow, w, h);
        assert!(
            glow_halo > plain_halo + 500,
            "glow must widen the soft halo (plain {plain_halo}, glow {glow_halo})"
        );
    }

    /// The glow layer is shifted by `glow_offset_x`: the leftmost halo pixel
    /// must move in the same direction as the offset.
    #[test]
    fn glow_offset_shifts_the_halo() {
        let (w, h) = (900, 400);

        fn leftmost_halo(w: i32, h: i32, theme: &Theme) -> i32 {
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
            {
                let cr = cairo::Context::new(&s).expect("context");
                let state = DrawState {
                    quote: &test_quote(),
                    glitch: false,
                    glitch_dx: 0.0,
                    glitch_dy: 0.0,
                    glitch_echo_px: 0.0,
                    glitch_invert: false,
                    glitch_ghost: false,
                    typewriter: test_quote().text.chars().count(),
                    cursor_on: true,
                };
                draw(&cr, w, h, &state, theme);
            }
            let data = s.data().expect("surface data");
            let mut left = w;
            for y in (h / 2 - 60)..(h / 2 + 60) {
                for x in 0..w {
                    let pl = (y * w as i32 + x) as usize * 4;
                    if data[pl + 2] > 0x0C {
                        left = left.min(x);
                    }
                }
            }
            left
        }

        let mut left_theme = test_theme();
        left_theme.glow_radius = 10.0;
        left_theme.glow_alpha = 0.6;
        left_theme.glow_offset_x = -80.0;
        left_theme.glow_offset_y = 0.0;
        let mut right_theme = left_theme.clone();
        right_theme.glow_offset_x = 80.0;

        let left = leftmost_halo(w, h, &left_theme);
        let right = leftmost_halo(w, h, &right_theme);
        assert!(
            left < right - 40,
            "glow offset must shift the halo (leftmost {left} vs {right})"
        );
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
            glitch_dx: 42.0,
            glitch_dy: 3.0,
            glitch_echo_px: 55.0,
            glitch_invert: false,
            glitch_ghost: false,
            typewriter: test_quote().text.chars().count(),
            cursor_on: true,
        };
        draw(&cr, w, h, &state, &theme);

        let mut file = std::fs::File::create("/tmp/render_test.png").expect("create png");
        surf.as_ref().write_to_png(&mut file).expect("png");
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
            glitch_echo_px: 0.0,
            glitch_invert: false,
            glitch_ghost: false,
            typewriter: test_quote().text.chars().count(),
            cursor_on: true,
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

    /// Glitch strobe step: `glitch_ghost` blanks the block, so the center band
    /// must contain almost no bright pixels (text is not drawn).
    #[test]
    fn glitch_ghost_blanks_text() {
        let (w, h) = (900, 400);
        let mut surf = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");

        let theme = test_theme();
        let state = DrawState {
            quote: &test_quote(),
            glitch: true,
            glitch_dx: 30.0,
            glitch_dy: 0.0,
            glitch_echo_px: 0.0,
            glitch_invert: false,
            glitch_ghost: true,
            typewriter: test_quote().text.chars().count(),
            cursor_on: true,
        };
        {
            let cr = cairo::Context::new(&surf).expect("context");
            draw(&cr, w, h, &state, &theme);
        }

        let data = surf.data().expect("surface data");
        let mut bright = 0i64;
        for y in (h / 2 - 30)..(h / 2 + 30) {
            for x in 0..w {
                let pl = (y * w as i32 + x) as usize * 4;
                // >0xA0 red = the quote's white glyphs (orange author-shadow
                // and cyan author ink sit below this).
                if data[pl + 2] > 0xA0 {
                    bright += 1;
                }
            }
        }
        assert!(
            bright < 10,
            "ghost glitch step should blank the text, got {bright} bright pixels"
        );
    }

    /// Glitch invert flash: the block is drawn as dark ink over a bright bar,
    /// so the center band has a large bright area (the bar) but few pure-white
    /// glyph pixels.
    #[test]
    fn glitch_invert_flashes_bright() {
        let (w, h) = (900, 400);
        let mut surf = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");

        let theme = test_theme();
        let state = DrawState {
            quote: &test_quote(),
            glitch: true,
            glitch_dx: 10.0,
            glitch_dy: 0.0,
            glitch_echo_px: 0.0,
            glitch_invert: true,
            glitch_ghost: false,
            typewriter: test_quote().text.chars().count(),
            cursor_on: true,
        };
        {
            let cr = cairo::Context::new(&surf).expect("context");
            draw(&cr, w, h, &state, &theme);
        }

        let data = surf.data().expect("surface data");
        let mut bright = 0i64;
        for y in (h / 2 - 40)..(h / 2 + 40) {
            for x in 0..w {
                let pl = (y * w as i32 + x) as usize * 4;
                let b = data[pl].max(data[pl + 1]).max(data[pl + 2]);
                if b >= 90 {
                    bright += 1;
                }
            }
        }
        assert!(
            bright > 2000,
            "invert glitch should flash a bright bar, got {bright} bright pixels"
        );
    }

    /// One-shot typewriter reveal: `typewriter = 0` draws no glyphs (blank
    /// start), a partial count draws fewer white pixels than the fully
    /// revealed phrase (and more than zero).
    #[test]
    fn typewriter_reveals_gradually() {
        let (w, h) = (900, 400);
        let theme = test_theme();

        fn white_ink(surf: &mut cairo::ImageSurface, w: i32, h: i32) -> i64 {
            let data = surf.data().expect("surface data");
            let mut n = 0i64;
            for y in (h / 2 - 80)..(h / 2 + 80) {
                for x in 0..w {
                    let pl = (y * w as i32 + x) as usize * 4;
                    // >0xB0 red: crisp white quote glyph cores (pink author ink
                    // at ~0xA0 and the orange shadow fall below this cutoff).
                    if data[pl + 2] > 0xB0 {
                        n += 1;
                    }
                }
            }
            n
        }

        let render = |typed: usize| -> cairo::ImageSurface {
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
            {
                let cr = cairo::Context::new(&s).expect("context");
                let state = DrawState {
                    quote: &test_quote(),
                    glitch: false,
                    glitch_dx: 0.0,
                    glitch_dy: 0.0,
                    glitch_echo_px: 0.0,
                    glitch_invert: false,
                    glitch_ghost: false,
                    typewriter: typed,
                    cursor_on: true,
                };
                draw(&cr, w, h, &state, &theme);
            }
            s
        };

        let total = test_quote().text.chars().count();
        let empty = white_ink(&mut render(0), w, h);
        let partial = white_ink(&mut render(total / 3), w, h);
        let full = white_ink(&mut render(total), w, h);
        assert!(
            empty < 10,
            "typed=0 should reveal no white glyphs, got {empty}"
        );
        assert!(
            partial > 10 && partial < full,
            "partial reveal {partial} must show some but fewer white pixels than full {full}"
        );

        // Strictly increasing at multiple checkpoints: each step reveals
        // substantially more glyph content (not just cursor movement). The
        // old clip bug produced ~0 displaced pixels at every partial step.
        let q = total / 4;
        let hq = total / 2;
        let almost = total.saturating_sub(1);
        let a = white_ink(&mut render(q), w, h);
        let b = white_ink(&mut render(hq), w, h);
        let c = white_ink(&mut render(almost), w, h);
        assert!(b > a * 4 / 3, "typed={hq} ({b}) must be substantially brighter than typed={q} ({a})");
        assert!(c > b * 4 / 3, "typed={almost} ({c}) must be substantially brighter than typed={hq} ({b})");

        // Wrapped layout: narrow window forces 2+ lines; the reveal must
        // still show progressive growth (the bug was invisible clip on
        // centered/wrapped lines).
        let (nw, nh) = (400, 400);
        let render_narrow = |typed: usize| {
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, nw, nh).expect("surface");
            {
                let cr = cairo::Context::new(&s).expect("context");
                let state = DrawState {
                    quote: &test_quote(),
                    glitch: false,
                    glitch_dx: 0.0,
                    glitch_dy: 0.0,
                    glitch_echo_px: 0.0,
                    glitch_invert: false,
                    glitch_ghost: false,
                    typewriter: typed,
                    cursor_on: true,
                };
                draw(&cr, nw, nh, &state, &theme);
            }
            s
        };
        let nw_empty = white_ink(&mut render_narrow(0), nw, nh);
        let nw_mid   = white_ink(&mut render_narrow(total / 2), nw, nh);
        let nw_full  = white_ink(&mut render_narrow(total), nw, nh);
        assert!(nw_empty < 10, "narrow typed=0: got {nw_empty}");
        assert!(nw_mid > nw_empty + 500, "narrow typed={hq} ({nw_mid}) must show glyphs");
        assert!(nw_full > nw_mid + 500, "narrow typed={total} ({nw_full}) must be brighter than {nw_mid}");
    }

    /// The blink toggle gates the terminal caret on the no-overlay path: with
    /// `cursor_on = false` the pink block (and only the block) must disappear.
    #[test]
    fn cursor_blink_hides_caret() {
        let (w, h) = (900, 400);
        // animated_scanlines=true routes the caret to the main layer (the
        // overlay path is exercised live).  scanline_alpha=0 keeps the static
        // mesh from darkening the sampled band.
        let mut theme = test_theme();
        theme.animated_scanlines = true;
        theme.scanline_alpha = 0.0;

        let render = |cursor_on: bool| -> cairo::ImageSurface {
            let total = test_quote().text.chars().count();
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
            {
                let cr = cairo::Context::new(&s).expect("context");
                let state = DrawState {
                    quote: &test_quote(),
                    glitch: false,
                    glitch_dx: 0.0,
                    glitch_dy: 0.0,
                    glitch_echo_px: 0.0,
                    glitch_invert: false,
                    glitch_ghost: false,
                    // During the reveal the caret is the full-width block a
                    // blinking terminal shows at the typing position.
                    typewriter: total.saturating_sub(1),
                    cursor_on,
                };
                draw(&cr, w, h, &state, &theme);
            }
            s
        };

        // Count pixels that differ between the on/off frames: everything
        // (background, glow, text, author) is byte-identical except the caret
        // block, so the diff is exactly its area.
        let mut on = render(true);
        let mut off = render(false);
        let data_on = on.data().expect("surface data");
        let data_off = off.data().expect("surface data");
        let mut diff = 0i64;
        for y in 0..h {
            for x in 0..w {
                let pl = (y * w as i32 + x) as usize * 4;
                let a = data_on[pl].max(data_on[pl + 1]).max(data_on[pl + 2]);
                let b = data_off[pl].max(data_off[pl + 1]).max(data_off[pl + 2]);
                let a = a as i32;
                let b = b as i32;
                if a > b + 12 || b > a + 12 {
                    diff += 1;
                }
            }
        }
        assert!(
            diff > 400,
            "cursor_on must add a substantial caret block (diff={diff})"
        );
    }
}