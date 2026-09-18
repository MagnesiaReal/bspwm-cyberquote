//! Cairo/Pango paint pipeline for the quote ticker.
//!
//! Cost model: a full frame only redraws on a quote change, during the one-shot
//! typewriter reveal (~30 ms/char until the phrase is fully shown), during a
//! short glitch burst, or on window (re)allocation.  The one continuous
//! animation — the migrating scanlines — is drawn by `draw_scanline_tile` on a
//! separate transparent overlay: the mesh is pre-rendered once into an A8 tile
//! one period tall, and each frame is a single `Extend::Repeat` pattern paint (a
//! cached-pixmap blit), never the text layer.  When that overlay exists it
//! ALSO paints the blinking terminal caret (via `cursor_rect`, recolored to
//! `accent.cursor_color`) so its ~2 Hz blink costs one small rect fill per
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
use rand::Rng;

use crate::quotes::Quote;

/// Everything the renderer needs to know about the active look.  Built once
/// per app start from `config::Config`.
#[derive(Clone)]
pub struct Theme {
    /// Background RGB.
    pub bg: (f64, f64, f64),
    /// Foreground (quote body) RGB.
    pub fg: (f64, f64, f64),
    /// Glitch chromatic-echo color A RGB (the old `accent.cyan`; left echo).
    pub color_a: (f64, f64, f64),
    /// Glitch chromatic-echo color B RGB (the old `accent.magenta`; right echo).
    pub color_b: (f64, f64, f64),
    /// Accent orange RGB (author accent fallback).
    pub orange: (f64, f64, f64),
    /// Pango font family string (e.g. "JetBrains Mono").
    pub font_family: String,
    /// Body font size in CSS px.
    pub font_px: f64,
    /// Author (attribution) font size in CSS px.  When equal to `font_px` both
    /// texts use the same size (the historical default).
    pub author_font_px: f64,
    /// How each line is justified inside the (screen-centered) text block.
    pub text_alignment: pango::Alignment,
    /// Scanline darkness 0..1.
    pub scanline_alpha: f64,
    /// Scanline density: lines per screen height.
    pub scanline_lines: u32,
    /// Physical scanline thickness in px.  Driven by the `scanline_size_rem`
    /// knob so changing the config actually changes the mesh (before this the
    /// value only reached the CSS variables and the cairo mesh kept a fixed
    /// 35% thickness).
    pub scanline_size_px: f64,
    /// False when the animated scanline overlay is active (draw() then skips
    /// its own static scanlines to avoid a double-darkened mesh).
    pub animated_scanlines: bool,
    /// Glitch displacement intensity 0..1.
    pub glitch_intensity: f64,
    /// Author ink RGB (the `accent.author_color` knob).
    pub author_ink: (f64, f64, f64),
    /// Terminal caret RGB (the `accent.cursor_color` knob; defaults to match
    /// `author_ink`).  The blink overlay/fallback paints the caret in this.
    pub cursor_ink: (f64, f64, f64),
    /// Text glow: soft luminous halo color RGB (the `glow.color` knob).
    pub glow_color: (f64, f64, f64),
    /// Glow opacity 0..1 (0 disables the halo entirely).
    pub glow_intensity: f64,
    /// Glow blur radius in px: how far the halo spreads past the glyph edges.
    pub glow_radius: f64,
    /// Stroke/outline thickness in px added to the underlay text before blur.
    pub glow_thickness: f64,
    /// Squares: master switch.
    pub squares_enabled: bool,
    /// Squares: base color RGB (also tints the birth gradient).
    pub squares_color: (f64, f64, f64),
    /// Squares: peak alpha 0..1.
    pub squares_opacity: f64,
    /// Squares: drift speed in px/sec upward.
    pub squares_speed: f64,
    /// Squares: hard cap on concurrently-live particles.
    pub squares_max: u32,
    /// Squares: chance per 50 ms tick of spawning (0..1).
    pub squares_spawn_rate: f64,
    /// Squares: lower bound for random glitch-interval (sec).
    pub squares_glitch_min_s: f64,
    /// Squares: upper bound for random glitch-interval (sec).
    pub squares_glitch_max_s: f64,
    /// Squares: burst length in overlay ticks (~50 ms each).
    pub squares_glitch_ticks: u32,
    /// Bottom "birth" gradient switch.
    pub birth_gradient: bool,
    /// Birth gradient height as a fraction of screen height (0..1).
    pub birth_gradient_height: f64,
    /// Birth gradient opacity (0..1).
    pub birth_gradient_intensity: f64,
}

/// Cache for the pre-rendered static glow halo of the quote body.
///
/// Building the halo (a stroke-thickened underlay blurred into an A8 mask, then
/// recolored into an ARGB32 surface) is the expensive part of the glow; it
/// depends only on the phrase, window size and the theme's glow settings. The
/// cache is rebuilt once per phrase and every other frame just blits the stored
/// halo with a single fast `set_source_surface` + `paint()` — no per-frame
/// `mask()` rasterization (the CPU hog). A glitch burst intentionally skips the
/// blit for the burst duration, so glitch frames never pay the glow cost.
pub struct GlowCache {
    key: (u32, u32, String, u32),
    mask: Option<cairo::ImageSurface>,
    /// The halo recolored into an ARGB32 surface with the glow color/opacity
    /// baked in, ready to be blitted directly to the frame.
    halo: Option<cairo::ImageSurface>,
    /// Absolute device top-left of `halo` for this window/phrase.
    halo_pos: (f64, f64),
}

impl GlowCache {
    pub fn new() -> Self {
        Self {
            key: (0, 0, String::new(), 0),
            mask: None,
            halo: None,
            halo_pos: (0.0, 0.0),
        }
    }
}

/// Cache for the pre-rendered steady-state text block (glow + quote + author).
///
/// When the phrase, window size, font, or colours change the cache is rebuilt
/// once; every subsequent steady-state frame blits the stored surface with a
/// single `paint()` instead of recreating Pango layouts, rasterizing text, and
/// compositing the glow halo.  A glitch burst or typewriter reveal bypasses the
/// cache entirely and renders the old way; when they end the cache is rebuilt.
pub struct TextSurfaceCache {
    key: (u64, String, String, String, u64, u32, u32, u32, u32, u32, u32, u32),
    surface: Option<cairo::ImageSurface>,
    /// Top-left of the cached region in window coordinates.
    surface_pos: (f64, f64),
    /// Cached cursor rect `(cx, cy, cw, ch)` for the no-compositor fallback.
    cursor_rect: Option<(f64, f64, f64, f64)>,
}

impl TextSurfaceCache {
    pub fn new() -> Self {
        Self {
            key: (0, String::new(), String::new(), String::new(), 0, 0, 0, 0, 0, 0, 0, 0),
            surface: None,
            surface_pos: (0.0, 0.0),
            cursor_rect: None,
        }
    }
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
    /// fully revealed (author + end cursor) and rendering is `static`.
    pub typewriter: usize,
    /// Cursor visibility for the blink phase (`false` = the block caret is
    /// hidden this tick). The main layer honors it only when it owns the
    /// cursor (`theme.animated_scanlines`, i.e. no RGBA overlay); with a
    /// compositor the caret is painted on the scanline overlay instead.
    pub cursor_on: bool,
    /// Mutable cache for the static glow halo (see [`GlowCache`]). `None`
    /// disables the cached glow pipeline entirely (no halo is drawn).
    pub glow: Option<&'a mut GlowCache>,
    /// Mutable cache for the steady-state text block surface (see
    /// [`TextSurfaceCache`]). `None` disables the cached text pipeline entirely.
    pub text_cache: Option<&'a mut TextSurfaceCache>,
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

/// Map a config alignment string (`"left"`, `"right"`, `"center"`) to a Pango
/// alignment.  Unknown or missing values fall back to Center — the historical
/// default and the current look.
pub fn text_alignment(s: &str) -> pango::Alignment {
    match s.trim().to_ascii_lowercase().as_str() {
        "left" => pango::Alignment::Left,
        "right" => pango::Alignment::Right,
        _ => pango::Alignment::Center,
    }
}

/// Compact a `pango::Alignment` into a few bits for the text-surface cache key.
fn alignment_bits(a: pango::Alignment) -> u64 {
    match a {
        pango::Alignment::Left   => 1,
        pango::Alignment::Center => 2,
        pango::Alignment::Right  => 3,
        _                        => 0,
    }
}

/// Horizontal position of the author line.  For centered text the author is
/// independently centered at `w/2`.  For left/right justification it anchors
/// to the quote block's visual left/right edge, so the attribution rides along
/// with the justified quote instead of drifting back to screen-center.
///
/// Params: quote ink width (`iw`), author ink left-overhang (`aink_x`) and ink
/// width (`aw`).
fn author_x(align: pango::Alignment, w: f64, iw: f64, aink_x: f64, aw: f64) -> f64 {
    match align {
        pango::Alignment::Left => w / 2.0 - iw / 2.0 - aink_x,
        pango::Alignment::Right => w / 2.0 + iw / 2.0 - aink_x - aw,
        _ => w / 2.0 - aink_x - aw / 2.0,
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Paint one full frame for a `w × h` monitor surface.
pub fn draw(
    cr: &cairo::Context,
    w: i32,
    h: i32,
    state: &mut DrawState,
    theme: &Theme,
) {
    let (fw, fh) = (w as f64, h as f64);
    let (br, bg, bb) = theme.bg;

    // 1. background
    cr.set_source_rgba(br, bg, bb, 1.0);
    cr.paint();

    // 1b. bottom "birth" glow where the squares emerge (static, cheap)
    if theme.birth_gradient
        && theme.birth_gradient_intensity > 0.0
        && theme.birth_gradient_height > 0.0
    {
        draw_birth_gradient(cr, fw, fh, theme);
    }

    // 2. text block (quote + author), centered slightly above middle
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
    state: &mut DrawState,
    theme: &Theme,
) -> (f64, f64) {
    let wrap = (w * WRAP_FRACTION).max(120.0);

    let total_chars = state.quote.text.chars().count();
    let typed = state.typewriter.min(total_chars);
    let typing = typed < total_chars;

    // ---- Text surface cache key (everything that affects the steady-state block) ----
    let fg = theme.fg;
    let ai = theme.author_ink;
    let gi = (theme.glow_intensity * 1000.0).round() as u32;
    let dims = ((w as u64) << 32) | (h as u64);
    let fonts = ((theme.font_px * 100.0).round() as u64) << 32
        | (theme.author_font_px * 100.0).round() as u64
        | (alignment_bits(theme.text_alignment) << 56);
    let text_key = (
        dims,
        state.quote.text.clone(),
        state.quote.author.clone(),
        theme.font_family.clone(),
        fonts,
        (fg.0 * 1000.0) as u32,
        (fg.1 * 1000.0) as u32,
        (fg.2 * 1000.0) as u32,
        (ai.0 * 1000.0) as u32,
        (ai.1 * 1000.0) as u32,
        (ai.2 * 1000.0) as u32,
        gi,
    );

    // ---- Cache hit: blit the pre-rendered surface and bail out ----
    if !state.glitch && !typing {
        if let Some(tc) = state.text_cache.as_deref() {
            if tc.key == text_key {
                if let Some(surf) = &tc.surface {
                    cr.save();
                    cr.set_source_surface(surf, tc.surface_pos.0, tc.surface_pos.1);
                    cr.paint();
                    cr.restore();
                    // Cursor (no-compositor fallback only)
                    if theme.animated_scanlines && state.cursor_on {
                        if let Some((cx, cy, cw, ch)) = tc.cursor_rect {
                            let (crr, cgg, cbb) = theme.cursor_ink;
                            cr.set_source_rgba(crr, cgg, cbb, 0.9);
                            cr.rectangle(cx, cy, cw, ch);
                            cr.fill();
                        }
                    }
                    return (0.0, 0.0);
                }
            }
        }
    }

    // ---- Cache miss or non-cacheable state: full render path ----
    let quote_layout = make_layout(cr, theme, &state.quote.text, wrap, theme.font_px);
    let author_layout = make_layout(cr, theme, &format!("— {}", state.quote.author), wrap, theme.author_font_px);

    let (qink, _qlog) = quote_layout.pixel_extents();
    let (aink, _alog) = author_layout.pixel_extents();
    let (iw, ih) = (qink.width() as f64, qink.height() as f64);
    let (aw, ah) = (aink.width() as f64, aink.height() as f64);

    let total = ih + BLOCK_GAP_PX + ah;
    let top = (h - total) / 2.0 - h * 0.04;

    let qx = w / 2.0 - qink.x() as f64 - iw / 2.0;
    let qy = top - qink.y() as f64;
    let ax = author_x(theme.text_alignment, w, iw, aink.x() as f64, aw);
    let ay = top + ih + BLOCK_GAP_PX - aink.y() as f64;

    let glow_halo = if theme.glow_intensity > 0.01 {
        if let Some(gc) = state.glow.as_deref_mut() {
            let intensity_bits = (theme.glow_intensity * 1000.0).round() as u32;
            let key = (w as u32, h as u32, state.quote.text.clone(), intensity_bits);
            if gc.key != key {
                gc.key = key;
                gc.mask = build_glow_mask(&quote_layout, theme);
                gc.halo = gc.mask.as_ref().and_then(|m| build_halo_surface(m, theme));
                gc.halo_pos = {
                    let pad_sc = glow_pad_scaled(theme) / GLOW_SCALE;
                    (
                        qx + qink.x() as f64 - pad_sc,
                        qy + qink.y() as f64 - pad_sc,
                    )
                };
            }
        }
        state
            .glow
            .as_ref()
            .and_then(|gc| gc.halo.as_ref().map(|h| (h, gc.halo_pos)))
    } else {
        None
    };

    // Track whether the author line was already composited into the cache
    // surface so we don't draw it a second time on the frame.
    let mut skip_author = false;

    if state.glitch {
        let ox = qx + state.glitch_dx;
        let oy = qy + state.glitch_dy;
        if !state.glitch_ghost {
            let echo = state.glitch_echo_px;
            if echo > 0.5 {
                draw_text(cr, &quote_layout, ox + echo, oy, theme.color_b, 0.85);
                draw_text(cr, &quote_layout, ox - echo, oy, theme.color_a, 0.85);
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
        let byte = char_byte_index(&state.quote.text, typed);
        cr.save();
        push_typewriter_clip(cr, &quote_layout, byte, qx, qy);
        if let Some((halo, (lx, ly))) = glow_halo {
            blit_glow_halo(cr, halo, lx, ly);
        }
        draw_text(cr, &quote_layout, qx, qy, theme.fg, 1.0);
        cr.restore();
        if theme.animated_scanlines && state.cursor_on {
            draw_terminal_cursor(cr, &quote_layout, qx, qy, byte, theme);
        }
    } else {
        // Steady state: render the block into a cache surface, blit it, and
        // store it for subsequent frames.
        let glow_bounds = glow_halo.map(|(h, (lx, ly))| {
            (
                lx,
                ly,
                lx + h.width() as f64,
                ly + h.height() as f64,
            )
        });
        let quote_bounds = (
            qx + qink.x() as f64,
            qy + qink.y() as f64,
            qx + qink.x() as f64 + iw,
            qy + qink.y() as f64 + ih,
        );
        let author_bounds = (
            ax + aink.x() as f64,
            ay + aink.y() as f64,
            ax + aink.x() as f64 + aw,
            ay + aink.y() as f64 + ah,
        );

        let mut left = quote_bounds.0.min(author_bounds.0);
        let mut top_y = quote_bounds.1.min(author_bounds.1);
        let mut right = quote_bounds.2.max(author_bounds.2);
        let mut bottom = quote_bounds.3.max(author_bounds.3);

        if let Some((gl, gt, gr, gb)) = glow_bounds {
            left = left.min(gl);
            top_y = top_y.min(gt);
            right = right.max(gr);
            bottom = bottom.max(gb);
        }

        let sw = (right - left).ceil() as i32;
        let sh = (bottom - top_y).ceil() as i32;

        let mut used_cache = false;
        if sw > 0 && sh > 0 {
            if let Ok(surf) = cairo::ImageSurface::create(cairo::Format::ARgb32, sw, sh) {
                {
                    let sc = cairo::Context::new(&surf).expect("text cache context");
                    // Match the frame context's font options so the cached
                    // surface rasterizes text identically to the direct path.
                    if let Ok(fo) = cr.font_options() {
                        sc.set_font_options(&fo);
                    }
                    sc.translate(-left, -top_y);
                    if let Some((halo, (lx, ly))) = glow_halo {
                        blit_glow_halo(&sc, halo, lx, ly);
                    }
                    draw_text(&sc, &quote_layout, qx, qy, theme.fg, 1.0);
                    let (ar, ag, ab) = theme.author_ink;
                    draw_text_fade_author(&sc, &author_layout, ax, ay, (ar, ag, ab));
                }

                // Blit to frame.
                cr.save();
                cr.set_source_surface(&surf, left, top_y);
                cr.paint();
                cr.restore();

                // Cursor (no-compositor fallback)
                if theme.animated_scanlines && state.cursor_on {
                    draw_terminal_cursor(
                        cr,
                        &quote_layout,
                        qx,
                        qy,
                        quote_layout.text().len(),
                        theme,
                    );
                }

                // Compute cursor rect for future cache blits.
                let cursor_rect = if !theme.animated_scanlines {
                    let byte = quote_layout.text().len();
                    let rect = quote_layout.index_to_pos(byte as i32);
                    let sc2 = pango::SCALE as f64;
                    let cx = qx + rect.x() as f64 / sc2;
                    let cy = qy + rect.y() as f64 / sc2;
                    let cw = (rect.width() as f64 / sc2).clamp(2.0, theme.font_px * 0.7);
                    let ch = (rect.height() as f64 / sc2).max(2.0);
                    Some((cx, cy, cw, ch))
                } else {
                    None
                };

                if let Some(tc) = state.text_cache.as_deref_mut() {
                    tc.key = text_key;
                    tc.surface = Some(surf);
                    tc.surface_pos = (left, top_y);
                    tc.cursor_rect = cursor_rect;
                }
                used_cache = true;
                skip_author = true;
            }
        }

        // Fallback if surface creation failed.
        if !used_cache {
            if let Some((halo, (lx, ly))) = glow_halo {
                blit_glow_halo(cr, halo, lx, ly);
            }
            draw_text(cr, &quote_layout, qx, qy, theme.fg, 1.0);
            if theme.animated_scanlines && state.cursor_on {
                draw_terminal_cursor(
                    cr,
                    &quote_layout,
                    qx,
                    qy,
                    quote_layout.text().len(),
                    theme,
                );
            }
        }
    }

    // The author line joins the animation only once the quote body is fully
    // revealed. A glitch ghost strobe blanks the whole block, author included.
    // Skipped when the author was already composited into the cache surface.
    if !skip_author && !typing && !(state.glitch && state.glitch_ghost) {
        let (ar, ag, ab) = theme.author_ink;
        draw_text_fade_author(cr, &author_layout, ax, ay, (ar, ag, ab));
    }
    (0.0, 0.0)
}

/// Build a wrapped, centered Pango layout from a context.
fn make_layout(cr: &cairo::Context, theme: &Theme, text: &str, wrap_px: f64, font_px: f64) -> pango::Layout {
    let layout = pangocairo::create_layout(cr);
    let mut desc = pango::FontDescription::from_string(&theme.font_family);
    // CSS px → Pango size (Pango units are points * SCALE; 96 dpi ⇒ px * 0.75 pt)
    let points = font_px * 0.75;
    desc.set_size((points * pango::SCALE as f64) as i32);
    layout.set_font_description(Some(&desc));
    layout.set_width((wrap_px * pango::SCALE as f64) as i32);
    layout.set_alignment(theme.text_alignment);
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

/// Author line: a single crisp pass in the author color (no offset shadow).
fn draw_text_fade_author(
    cr: &cairo::Context,
    layout: &pango::Layout,
    x: f64,
    y: f64,
    color: (f64, f64, f64),
) {
    draw_text(cr, layout, x, y, color, 1.0);
}

// ---------------------------------------------------------------------------
// Text glow
// ---------------------------------------------------------------------------

/// Approximate Gaussian blur on an A8 buffer: 2 passes (horizontal + vertical)
/// of a clamped box filter with a sliding window. `kernel` is the box
/// half-width in pixels; clamped edges avoid dark halos at the boundary.
///
/// Operates purely on safe Rust slices — no aliasing through cairo's shared
/// `with_data` borrows, which the release optimizer used to exploit (mutation
/// through a `&[u8]` is UB and silently dropped the pixels).
fn box_blur(data: &mut [u8], w: usize, h: usize, stride: usize, kernel: f64) {
    let k = kernel.max(1.0).floor() as usize;
    if w == 0 || h == 0 {
        return;
    }
    let mut tmp = vec![0u8; h * stride];

    // Vertical pass: read `data`, write `tmp`.
    for x in 0..w {
        let mut sum: u64 = 0;
        let mut cnt: u64 = 0;
        let first = k.min(h - 1);
        for y in 0..=first {
            sum += data[y * stride + x] as u64;
            cnt += 1;
        }
        tmp[x] = (sum / cnt) as u8;
        for i in 1..h {
            if i + k < h {
                sum += data[(i + k) * stride + x] as u64;
                cnt += 1;
            }
            if i > k {
                sum -= data[(i - k - 1) * stride + x] as u64;
                cnt -= 1;
            }
            tmp[i * stride + x] = (sum / cnt.max(1)) as u8;
        }
    }

    // Horizontal pass: read `tmp`, write `data`.
    for y in 0..h {
        let row = y * stride;
        let mut sum: u64 = 0;
        let mut cnt: u64 = 0;
        let first = k.min(w - 1);
        for x in 0..=first {
            sum += tmp[row + x] as u64;
            cnt += 1;
        }
        data[row] = (sum / cnt) as u8;
        for x in 1..w {
            if x + k < w {
                sum += tmp[row + x + k] as u64;
                cnt += 1;
            }
            if x > k {
                sum -= tmp[row + x - k - 1] as u64;
                cnt -= 1;
            }
            data[row + x] = (sum / cnt.max(1)) as u8;
        }
    }
}

const GLOW_SCALE: f64 = 1.5;

/// Mask-space padding for the glow geometry: the underlay stroke growth plus
/// the blur reach, both in the scaled mask space, so neither clamps against
/// the mask boundary. `pad / GLOW_SCALE` is that padding in device px.
fn glow_pad_scaled(theme: &Theme) -> f64 {
    let sc = GLOW_SCALE;
    (theme.glow_thickness.max(0.0) * sc + theme.glow_radius.max(0.5) * sc * 0.9 + 3.0)
        .ceil()
        .max(8.0)
}

/// Build the reusable glow halo for *any* text layout. Per the recipe the
/// underlay is layered: (1) the same text rendered with a thicker outline —
/// the path stroked with round joins/caps (`glow_thickness`), (2) blurred into
/// a soft halo (`glow_radius` spread, 4 box passes ≈ a Gaussian), into an A8
/// mask at 1.5× scale; the crisp text is then drawn by the caller on top. The
/// halo's color/opacity are applied at composite time.
///
/// The returned mask is text-tight (padded only for the stroke + blur reach)
/// and entirely static for a given phrase/layout/geometry — callers cache it
/// so the expensive blur is paid once per phrase, not once per frame.
pub fn build_glow_mask(layout: &pango::Layout, theme: &Theme) -> Option<cairo::ImageSurface> {
    let (ink, _) = layout.pixel_extents();
    let (cw, ch) = (ink.width() as f64, ink.height() as f64);
    if cw < 1.0 || ch < 1.0 {
        return None;
    }
    let sc = GLOW_SCALE;
    let pad = glow_pad_scaled(theme);
    let bw = (cw * sc + 2.0 * pad).ceil() as i32;
    let bh = (ch * sc + 2.0 * pad).ceil() as i32;
    if bw <= 0 || bh <= 0 {
        return None;
    }

    // 1. Thicker underlay: the text path stroked (round joins/caps) then
    //    filled, so the silhouette extends `thickness` px on every side.
    let mask = cairo::ImageSurface::create(cairo::Format::A8, bw, bh).expect("glow mask surface");
    {
        let tc = cairo::Context::new(&mask).expect("glow mask context");
        tc.scale(sc, sc);
        tc.translate(-ink.x() as f64 + pad / sc, -ink.y() as f64 + pad / sc);
        pangocairo::layout_path(&tc, layout);
        tc.set_source_rgba(0.0, 0.0, 0.0, 1.0);
        tc.set_line_width(2.0 * theme.glow_thickness.max(0.0));
        tc.set_line_join(cairo::LineJoin::Round);
        tc.set_line_cap(cairo::LineCap::Round);
        tc.stroke_preserve();
        tc.fill();
    }

    // 2. Blur the underlay (4 cumulative box passes ≈ a Gaussian). All byte
    //    work happens in a plain `Vec` so the release build can't optimize
    //    away aliased writes; the blurred A8 surface is assembled from the
    //    finished buffer.
    let stride = mask.stride() as usize;
    let mut data = vec![0u8; stride * bh as usize];
    mask.flush();
    mask.with_data(|src| {
        data.copy_from_slice(src);
    })
    .expect("read glow mask for blur");
    let kernel = theme.glow_radius.max(0.5) * sc / 4.0;
    for _ in 0..4 {
        box_blur(&mut data, bw as usize, bh as usize, stride, kernel);
    }
    Some(
        cairo::ImageSurface::create_for_data(data, cairo::Format::A8, bw, bh, mask.stride())
            .expect("glow blurred mask surface"),
    )
}

/// Composite a cached glow mask under sharp text. The pattern maps device px →
/// pattern space with a 1/`GLOW_SCALE` scale, so the 1.5× mask lands exactly
/// text-sized at (`x`, `y`); `ink` is the layout's ink extents, which pin the
/// halo to the same anchor the sharp pass uses. Halo color/opacity come from
/// the theme, so one cached mask serves any applied intensity.
///
/// Together with [`build_glow_mask`] this lets any text layout get the glow:
/// build the mask once for the layout, then composite it (optionally inside a
/// clipping region) right before drawing the crisp text at the same `x,y`.
pub fn composite_glow_mask(
    cr: &cairo::Context,
    mask: &cairo::ImageSurface,
    x: f64,
    y: f64,
    ink: &pango::Rectangle,
    theme: &Theme,
) {
    let sc = GLOW_SCALE;
    let pad_sc = glow_pad_scaled(theme) / sc;
    let pat = cairo::SurfacePattern::create(mask);
    pat.set_extend(cairo::Extend::None);
    let px = x + ink.x() as f64 - pad_sc;
    let py = y + ink.y() as f64 - pad_sc;
    pat.set_matrix(cairo::Matrix::new(sc, 0.0, 0.0, sc, -px * sc, -py * sc));
    let (gr, gg, gb) = theme.glow_color;
    cr.set_source_rgba(gr, gg, gb, theme.glow_intensity.clamp(0.0, 1.0));
    cr.mask(&pat);
}

/// Blit a pre-baked halo surface onto the frame at its absolute device
/// (`lx`, `ly`). The halo is rasterized at device resolution during the build,
/// so this stays an identity-transform `paint()` restricted to an explicit
/// clip rect — cairo only composites the small halo region, never the whole
/// screen (the scale+transform variant forced a full-frame re-raster and
/// cost ~6× more CPU).
fn blit_glow_halo(cr: &cairo::Context, halo: &cairo::ImageSurface, lx: f64, ly: f64) {
    cr.save();
    cr.rectangle(lx, ly, halo.width() as f64, halo.height() as f64);
    cr.clip();
    cr.set_source_surface(halo, lx, ly);
    cr.paint();
    cr.restore();
}

/// Recolor an A8 glow mask into an ARGB32 surface with the glow color/opacity
/// baked into the alpha channel (premultiplied) and downscaled to device
/// resolution, so compositing it each frame is a tiny identity `paint()` — no
/// per-frame scaling or mask rasterization. Returns `None` only for an empty
/// surface.
fn build_halo_surface(mask: &cairo::ImageSurface, theme: &Theme) -> Option<cairo::ImageSurface> {
    let (bw, bh) = (mask.width(), mask.height());
    if bw <= 0 || bh <= 0 {
        return None;
    }
    let sc = GLOW_SCALE;
    // Colored halo at the 1.5× mask resolution (premultiplied ARGB). Built in a
    // plain `Vec` (BGRA order, no aliased `with_data` writes) so release builds
    // keep the pixels, then handed to cairo by ownership.
    let (gr, gg, gb) = theme.glow_color;
    let a = theme.glow_intensity.clamp(0.0, 1.0);
    let src_stride = mask.stride() as usize;
    let big_stride = cairo::Format::ARgb32.stride_for_width(bw as u32).expect("stride");
    mask.flush();
    let mut big_data = vec![0u8; big_stride as usize * bh as usize];
    mask.with_data(|src| {
        let (w, h) = (bw as usize, bh as usize);
        for y in 0..h {
            let srow = y * src_stride;
            let drow = y * big_stride as usize;
            for x in 0..w {
                let m = src[srow + x] as f64 / 255.0 * a;
                let o = drow + x * 4;
                big_data[o] = (gb * m * 255.0) as u8;
                big_data[o + 1] = (gg * m * 255.0) as u8;
                big_data[o + 2] = (gr * m * 255.0) as u8;
                big_data[o + 3] = (m * 255.0) as u8;
            }
        }
    })
    .expect("read glow mask for halo");
    let big = cairo::ImageSurface::create_for_data(big_data, cairo::Format::ARgb32, bw, bh, big_stride)
        .expect("glow halo surface");

    // Downscale to device resolution once (bilinear) so per-frame blits cost
    // the same as a small region paint with an identity transform.
    let dw = ((bw as f64) / sc).ceil() as i32;
    let dh = ((bh as f64) / sc).ceil() as i32;
    let halo = cairo::ImageSurface::create(cairo::Format::ARgb32, dw, dh)
        .expect("glow halo device surface");
    {
        let hc = cairo::Context::new(&halo).expect("glow halo device context");
        hc.scale(1.0 / sc, 1.0 / sc);
        hc.set_source_surface(&big, 0.0, 0.0);
        hc.paint();
    }
    Some(halo)
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

    let (crr, cgg, cbb) = theme.cursor_ink;
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
    let quote_layout = make_layout(cr, theme, text, wrap, theme.font_px);
    let author_layout = make_layout(cr, theme, &format!("— {author}"), wrap, theme.author_font_px);

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
    // The config `scanline_size_rem` knob controls the band thickness; kept
    // inside the pitch so it never overlaps the following gap.
    let thickness = theme.scanline_size_px.clamp(1.0, spacing * 0.9).max(1.0);

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
    // Same knob as the animated tile: the config's `scanline_size_rem` knob
    // finally reaches the static fallback too, so both meshes stay identical.
    let thickness = theme.scanline_size_px.clamp(1.0, spacing * 0.9).max(1.0);

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

/// Soft bottom band (background layer): a solid-to-transparent linear gradient
/// in the squares' color marking the "birth zone" the squares rise out of.
/// Static, so on the never-animated main layer it costs one tiny gradient fill
/// per (rare) main-layer repaint.
fn draw_birth_gradient(cr: &cairo::Context, w: f64, h: f64, theme: &Theme) {
    let band = (h * theme.birth_gradient_height.clamp(0.0, 1.0)).clamp(0.0, h);
    if band <= 0.0 {
        return;
    }
    let (r, g, b) = theme.squares_color;
    let grad = cairo::LinearGradient::new(0.0, h - band, 0.0, h);
    grad.add_color_stop_rgba(0.0, r, g, b, 0.0);
    grad.add_color_stop_rgba(1.0, r, g, b, theme.birth_gradient_intensity.clamp(0.0, 1.0));
    cr.set_source(&grad);
    cr.rectangle(0.0, h - band, w, band);
    let _ = cr.fill();
}

// ---------------------------------------------------------------------------
// Floating glitched squares
// ---------------------------------------------------------------------------

/// Smallest square side (px).
const SQUARE_MIN_PX: f64 = 5.0;
/// Largest square side as a fraction of the body font size.
const SQUARE_MAX_SCALE: f64 = 0.8;
/// The "reference" size, as a fraction of the font, that drifts exactly at the
/// configured `speed`; larger squares are proportionally faster (closer),
/// smaller ones slower (far away) — the size-parallax illusion.
const SQUARE_MED_REF_SCALE: f64 = 0.5;
/// Speed-ratio floor/ceiling (relative to the reference size) so the very
/// smallest/largest squares stay within a lively, not glacial/teleporting
/// range.
const SQUARE_MIN_SPEED_RATIO: f64 = 0.3;
const SQUARE_MAX_SPEED_RATIO: f64 = 1.6;

/// Lifetime alpha for a particle at normalized progress `t` (0..1): fade in
/// as it emerges from the birth zone, hold, then fade back out just before it
/// reaches the mid-screen target.
fn square_alpha(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.18 {
        t / 0.18
    } else if t > 0.70 {
        1.0 - (t - 0.70) / 0.30
    } else {
        1.0
    }
}

/// Size → speed ratio: proportional to the square's side vs the reference
/// size, clamped to the lively envelope.  Monotonic, so bigger always means
/// faster.
fn square_speed_ratio(size: f64, ref_size: f64) -> f64 {
    (size / ref_size.max(1.0)).clamp(SQUARE_MIN_SPEED_RATIO, SQUARE_MAX_SPEED_RATIO)
}

/// One floating square.
#[derive(Clone, Copy)]
pub struct Particle {
    /// Square side in px (random per particle).
    pub size: f64,
    pub x: f64,
    pub y: f64,
    pub start_y: f64,
    pub end_y: f64,
    /// Vertical drift in px/sec toward the top (proportional to `size`).
    pub speed: f64,
    /// Peak alpha for this particle (per-particle variety).
    pub max_alpha: f64,
    /// 0..1 progress from `start_y` to `end_y`.
    pub t: f64,
    /// Ticks until the next glitch burst may start.
    pub next_glitch: u32,
    /// Ticks remaining in the current glitch burst (0 = idle).
    pub glitch_ticks: u32,
    pub glitch_dx: f64,
    pub glitch_dy: f64,
    /// Re-rolled every glitch step, mirroring the text: the crisp white square
    /// swings sideways and sways slightly up/down...
    pub echo_a_dx: f64,
    pub echo_a_dy: f64,
    pub echo_b_dx: f64,
    pub echo_b_dy: f64,
}

/// Reset a particle's per-tick glitch offsets (when a burst ends).
fn reset_glitch(p: &mut Particle) {
    p.glitch_dx = 0.0;
    p.glitch_dy = 0.0;
    p.echo_a_dx = 0.0;
    p.echo_a_dy = 0.0;
    p.echo_b_dx = 0.0;
    p.echo_b_dy = 0.0;
}

/// Roll a fresh set of random offsets for one glitch step, mimicking the text
/// glitch: the white square jumps to a random side amplitude and sways a bit
/// vertically, while the color_a / color_b echoes appear at random individual
/// offsets — randomly "desfasadas" from the white square.  Echoes are usually
/// present but occasionally dropped for a cleaner step.
fn shake_square(p: &mut Particle, rng: &mut impl rand::Rng) {
    let amp = rng.gen_range(2.0..=10.0);
    p.glitch_dx = if rng.gen_bool(0.5) { amp } else { -amp };
    p.glitch_dy = rng.gen_range(-3.0..=3.0);
    if rng.gen_bool(0.85) {
        p.echo_a_dx = -rng.gen_range(3.0..=9.0);
        p.echo_a_dy = rng.gen_range(-3.0..=3.0);
        p.echo_b_dx = rng.gen_range(3.0..=9.0);
        p.echo_b_dy = rng.gen_range(-3.0..=3.0);
    } else {
        p.echo_a_dx = 0.0;
        p.echo_a_dy = 0.0;
        p.echo_b_dx = 0.0;
        p.echo_b_dy = 0.0;
    }
}

/// The per-window particle field.  Squares are drawn as direct sized rect
/// fills (they vary in size continuously, so there is nothing to pre-bake),
/// keeping the per-frame cost at a few tiny fills regardless of size.
pub struct SquareField {
    pub particles: Vec<Particle>,
}

impl SquareField {
    pub fn new() -> Self {
        Self { particles: Vec::new() }
    }
}

/// Roll one new square in from below the screen edge with a random size
/// (sqrt-biased toward the small end so far-away squares are the common case),
/// position, speed, alpha and travel target (around the mid-screen band).
fn spawn_square(w: f64, h: f64, theme: &Theme, rng: &mut impl rand::Rng) -> Particle {
    let hi = (theme.font_px * SQUARE_MAX_SCALE).max(SQUARE_MIN_PX + 1.0);
    // size = min + span * sqrt(U): uniform-in-probability per *area* — more
    // small far-away squares, fewer big close ones, no flat banding.
    let size = SQUARE_MIN_PX + (hi - SQUARE_MIN_PX) * rng.gen::<f64>().sqrt();
    let margin = 40.0 + rng.gen_range(0.0..=60.0);
    let start_y = h + margin;
    let end_y = h * 0.5 + rng.gen_range(-0.08..=0.08) * h;
    let ref_size = theme.font_px * SQUARE_MED_REF_SCALE;
    let speed = theme.squares_speed.max(1.0)
        * square_speed_ratio(size, ref_size)
        * rng.gen_range(0.8..=1.25);
    let gmin = (theme.squares_glitch_min_s.max(0.1) * 20.0) as u32;
    let gmax = (theme.squares_glitch_max_s.max(theme.squares_glitch_min_s) * 20.0).max(1.0) as u32;
    Particle {
        size,
        x: rng.gen_range(0.0..=w),
        y: start_y,
        start_y,
        end_y,
        speed,
        max_alpha: theme.squares_opacity.clamp(0.0, 1.0) * rng.gen_range(0.6..=1.0),
        t: 0.0,
        next_glitch: rng.gen_range(gmin..=gmax.max(gmin)),
        glitch_ticks: 0,
        glitch_dx: 0.0,
        glitch_dy: 0.0,
        echo_a_dx: 0.0,
        echo_a_dy: 0.0,
        echo_b_dx: 0.0,
        echo_b_dy: 0.0,
    }
}

/// Step the particle field forward `dt_sec` seconds (the 50 ms overlay tick):
/// drift each particle toward its target, spawn by probability up to the cap,
/// and drive the per-particle random glitch schedule.
pub fn update_square_field(field: &mut SquareField, w: i32, h: i32, dt_sec: f64, theme: &Theme) {
    if !theme.squares_enabled {
        if !field.particles.is_empty() {
            field.particles.clear();
        }
        return;
    }
    let (w, h) = ((w as f64).max(1.0), (h as f64).max(1.0));
    let dt = dt_sec.max(0.001);
    let gmin = (theme.squares_glitch_min_s.max(0.1) * 20.0) as u32;
    let gmax = (theme.squares_glitch_max_s.max(theme.squares_glitch_min_s) * 20.0).max(1.0) as u32;
    let spawn_rate = theme.squares_spawn_rate.clamp(0.0, 1.0);

    let mut rng = rand::thread_rng();

    for p in field.particles.iter_mut() {
        p.t += (p.speed * dt) / (p.start_y - p.end_y).abs().max(1.0);
        p.t = p.t.min(1.0 + 1e-6);
        p.y = p.start_y + (p.end_y - p.start_y) * p.t;

        if p.glitch_ticks > 0 {
            p.glitch_ticks -= 1;
            if p.glitch_ticks == 0 {
                reset_glitch(p);
            } else {
                // Re-roll every step like the text glitch: the square shudders
                // side-to-side with fresh random echoes each frame.
                shake_square(p, &mut rng);
            }
        } else if p.next_glitch == 0 {
            // Only glitch while the square is comfortably visible.
            if (0.2..=0.85).contains(&p.t) {
                p.glitch_ticks = theme.squares_glitch_ticks.max(1);
                shake_square(p, &mut rng);
            }
            let hi = gmax.max(gmin);
            p.next_glitch = rng.gen_range(gmin..=hi);
        } else {
            p.next_glitch -= 1;
        }
    }

    let live = field.particles.iter().filter(|p| p.t < 1.0).count();
    if (live as u32) < theme.squares_max.max(1) && rng.gen_bool(spawn_rate) {
        field.particles.push(spawn_square(w, h, theme, &mut rng));
    }
    field.particles.retain(|p| p.t < 1.0);
}

/// Paint the field on the overlay: each particle is a direct sized rect fill
/// at its lifetime alpha (a couple of tiny fills, independent of size); during
/// a glitch burst the color_a/color_b chromatic echoes appear at random
/// individual offsets from the shivering white square, mirroring the text
/// glitch.
pub fn draw_squares(cr: &cairo::Context, field: &SquareField, theme: &Theme) {
    if !theme.squares_enabled {
        return;
    }
    for p in &field.particles {
        let alpha = square_alpha(p.t) * p.max_alpha;
        if alpha <= 0.002 {
            continue;
        }
        let s = p.size;
        let (x, y) = (p.x + p.glitch_dx, p.y + p.glitch_dy);
        // Chromatic echoes with random individual offsets from the glitched
        // square — color_a trails on the left, color_b on the right, each also
        // straying slightly up/down, exactly like the text glitch's echo split.
        if p.echo_a_dx != 0.0 || p.echo_b_dx != 0.0 {
            let (ar, ag, ab) = theme.color_a;
            cr.set_source_rgba(ar, ag, ab, (alpha * 0.8).clamp(0.0, 1.0));
            cr.rectangle(x + p.echo_a_dx, y + p.echo_a_dy, s, s);
            let _ = cr.fill();
            let (br, bg, bb) = theme.color_b;
            cr.set_source_rgba(br, bg, bb, (alpha * 0.8).clamp(0.0, 1.0));
            cr.rectangle(x + p.echo_b_dx, y + p.echo_b_dy, s, s);
            let _ = cr.fill();
        }
        let (r, g, b) = theme.squares_color;
        cr.set_source_rgba(r, g, b, alpha.clamp(0.0, 1.0));
        cr.rectangle(x, y, s, s);
        let _ = cr.fill();
    }
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
            color_a: (0.22, 0.90, 1.0),
            color_b: (1.0, 0.24, 0.87),
            orange: (1.0, 0.89, 0.70),
            font_family: "monospace".into(),
            font_px: 24.0,
            author_font_px: 24.0,
            text_alignment: pango::Alignment::Center,
            scanline_alpha: 0.3,
            scanline_lines: 100,
            scanline_size_px: 1.5,
            animated_scanlines: false,
            glitch_intensity: 0.35,
            author_ink: (0.63, 0.16, 0.90),
            cursor_ink: (0.63, 0.16, 0.90),
            glow_color: (0.22, 0.90, 1.0),
            glow_intensity: 0.45,
            glow_radius: 4.0,
            glow_thickness: 2.0,
            squares_enabled: true,
            squares_color: (1.0, 1.0, 1.0),
            squares_opacity: 0.5,
            squares_speed: 34.0,
            squares_max: 48,
            squares_spawn_rate: 0.12,
            squares_glitch_min_s: 1.0,
            squares_glitch_max_s: 4.5,
            squares_glitch_ticks: 4,
            birth_gradient: true,
            birth_gradient_height: 0.25,
            birth_gradient_intensity: 0.28,
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
        let mut state = DrawState {
            quote: &test_quote(),
            glitch: true,
            glitch_dx: 42.0,
            glitch_dy: 3.0,
            glitch_echo_px: 55.0,
            glitch_invert: false,
            glitch_ghost: false,
            typewriter: test_quote().text.chars().count(),
            cursor_on: true,
            glow: None,
            text_cache: None,
        };
        draw(&cr, w, h, &mut state, &theme);

        let mut file = std::fs::File::create("/tmp/render_test.png").expect("create png");
        surf.as_ref().write_to_png(&mut file).expect("png");
    }

    /// The crisp text pass must actually change pixels in the center band.
    #[test]
    fn render_is_not_blank() {
        let (w, h) = (900, 400);
        let mut surf = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");

        let theme = test_theme();
        let mut state = DrawState {
            quote: &test_quote(),
            glitch: false,
            glitch_dx: 0.0,
            glitch_dy: 0.0,
            glitch_echo_px: 0.0,
            glitch_invert: false,
            glitch_ghost: false,
            typewriter: test_quote().text.chars().count(),
            cursor_on: true,
            glow: None,
            text_cache: None,
        };
        {
            let cr = cairo::Context::new(&surf).expect("context");
            draw(&cr, w, h, &mut state, &theme);
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
        let mut state = DrawState {
            quote: &test_quote(),
            glitch: true,
            glitch_dx: 30.0,
            glitch_dy: 0.0,
            glitch_echo_px: 0.0,
            glitch_invert: false,
            glitch_ghost: true,
            typewriter: test_quote().text.chars().count(),
            cursor_on: true,
            glow: None,
            text_cache: None,
        };
        {
            let cr = cairo::Context::new(&surf).expect("context");
            draw(&cr, w, h, &mut state, &theme);
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
        let mut state = DrawState {
            quote: &test_quote(),
            glitch: true,
            glitch_dx: 10.0,
            glitch_dy: 0.0,
            glitch_echo_px: 0.0,
            glitch_invert: true,
            glitch_ghost: false,
            typewriter: test_quote().text.chars().count(),
            cursor_on: true,
            glow: None,
            text_cache: None,
        };
        {
            let cr = cairo::Context::new(&surf).expect("context");
            draw(&cr, w, h, &mut state, &theme);
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
                let mut state = DrawState {
                    quote: &test_quote(),
                    glitch: false,
                    glitch_dx: 0.0,
                    glitch_dy: 0.0,
                    glitch_echo_px: 0.0,
                    glitch_invert: false,
                    glitch_ghost: false,
                    typewriter: typed,
                    cursor_on: true,
                    glow: None,
                    text_cache: None,
                };
                draw(&cr, w, h, &mut state, &theme);
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
                let mut state = DrawState {
                    quote: &test_quote(),
                    glitch: false,
                    glitch_dx: 0.0,
                    glitch_dy: 0.0,
                    glitch_echo_px: 0.0,
                    glitch_invert: false,
                    glitch_ghost: false,
                    typewriter: typed,
                    cursor_on: true,
                    glow: None,
                    text_cache: None,
                };
                draw(&cr, nw, nh, &mut state, &theme);
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
                let mut state = DrawState {
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
                    glow: None,
                    text_cache: None,
                };
                draw(&cr, w, h, &mut state, &theme);
            }
            s
        };

        // Count pixels that differ between the on/off frames: everything
        // (background, text, author) is byte-identical except the caret
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

    /// The glow halo: rendering the same (steady, no-glitch) frame with the
    /// glow enabled must add a substantial band of cyan-tinted halo pixels
    /// around the crisp text that is absent when the glow is off.  The halo is
    /// semi-transparent, so overlapping crisp white glyph cores stay white.
    /// A glitch frame must NOT composite the static glow layer (the lag-fix
    /// strategy), so it carries no more halo than the glow-off frame.
    #[test]
    fn glow_adds_halo_pixels() {
        let (w, h) = (900, 400);
        let base_theme = test_theme();

        // base_theme.glow_color == (0.22, 0.90, 1.0) cyan.
        let mut cache = GlowCache::new();
        let render = |glow_intensity: f64, glitch: bool, cache: &mut GlowCache| -> cairo::ImageSurface {
            let mut theme = base_theme.clone();
            theme.glow_intensity = glow_intensity;
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
            {
                let cr = cairo::Context::new(&s).expect("context");
                let mut state = DrawState {
                    quote: &test_quote(),
                    glitch,
                    glitch_dx: 0.0,
                    glitch_dy: 0.0,
                    glitch_echo_px: 0.0,
                    glitch_invert: false,
                    glitch_ghost: false,
                    typewriter: test_quote().text.chars().count(),
                    cursor_on: false,
                    glow: Some(cache),
                    text_cache: None,
                };
                draw(&cr, w, h, &mut state, &theme);
            }
            s
        };

        fn cyan_halo(surf: &mut cairo::ImageSurface, w: i32, h: i32) -> i64 {
            // Premultiplied BGRA: bytes [B,G,R,A].  Count pixels that are
            // clearly cyan-tinted (halo color at the test intensity over the
            // dark background); crisp white text and background never match.
            let data = surf.data().expect("surface data");
            let mut n = 0i64;
            for y in 0..h {
                for x in 0..w {
                    let pl = (y * w as i32 + x) as usize * 4;
                    let b = data[pl] as i32;
                    let g = data[pl + 1] as i32;
                    let r = data[pl + 2] as i32;
                    if b > 40 && g > 30 && r < 90 && (b - r) > 30 {
                        n += 1;
                    }
                }
            }
            n
        }

        // Reuse one cache across renders: the halo mask is built once (first
        // call) and only composited with the requested intensity afterwards.
        let steady = cyan_halo(&mut render(0.45, false, &mut cache), w, h);
        let no_glow = cyan_halo(&mut render(0.0, false, &mut cache), w, h);
        assert!(
            steady > no_glow + 200,
            "glow must add a cyan halo ({steady} px) well beyond the no-glow frame ({no_glow} px)"
        );
        // Lag fix: while a glitch burst is active the static glow layer is
        // hidden, so the frame has essentially no halo despite glow being on.
        let glitching = cyan_halo(&mut render(0.45, true, &mut cache), w, h);
        assert!(
            glitching <= no_glow + 20,
            "glitch frames must hide the glow layer ({glitching} halo px vs no-glow {no_glow} px)"
        );
    }

    /// The text surface cache must reproduce the full (non-cached) render
    /// pixel-for-pixel: first frame builds the surface, second frame blits it.
    #[test]
    fn text_surface_cache_matches_full_render() {
        let (w, h) = (900, 400);
        let base = test_theme();
        let quote = test_quote();

        fn render(
            tc: Option<&mut TextSurfaceCache>,
            q: &Quote,
            w: i32,
            h: i32,
            theme: &Theme,
        ) -> cairo::ImageSurface {
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
            {
                let cr = cairo::Context::new(&s).expect("context");
                let mut state = DrawState {
                    quote: q,
                    glitch: false,
                    glitch_dx: 0.0,
                    glitch_dy: 0.0,
                    glitch_echo_px: 0.0,
                    glitch_invert: false,
                    glitch_ghost: false,
                    typewriter: q.text.chars().count(),
                    cursor_on: false,
                    glow: None,
                    text_cache: tc,
                };
                draw(&cr, w, h, &mut state, theme);
            }
            s
        }

        let mut full = render(None, &quote, w, h, &base);
        let mut tc = TextSurfaceCache::new();
        let mut build = render(Some(&mut tc), &quote, w, h, &base);
        let mut blit = render(Some(&mut tc), &quote, w, h, &base);
        assert_eq!(
            build.data().expect("surface").to_vec(),
            full.data().expect("surface").to_vec(),
            "cache-build frame must equal the full render"
        );
        assert_eq!(
            blit.data().expect("surface").to_vec(),
            full.data().expect("surface").to_vec(),
            "cache-blit frame must equal the full render"
        );

        // A different phrase must invalidate the cache and rebuild, not blit
        // a stale surface.
        let q2 = Quote {
            text: "A longer, completely different phrase that wraps over multiple lines so the bounds math is exercised as well.".to_string(),
            author: "Different Author".to_string(),
        };
        let mut tc = TextSurfaceCache::new();
        let mut alt = render(Some(&mut tc), &q2, w, h, &base);
        assert_ne!(
            alt.data().expect("surface").to_vec(),
            full.data().expect("surface").to_vec(),
            "a different phrase must invalidate the text surface cache"
        );
    }

    /// The `text_alignment` helper maps config strings to Pango enums, and
    /// an unknown value falls back to Center.
    #[test]
    fn alignment_parse() {
        assert_eq!(text_alignment("left"),  pango::Alignment::Left);
        assert_eq!(text_alignment("right"), pango::Alignment::Right);
        assert_eq!(text_alignment("center"), pango::Alignment::Center);
        assert_eq!(text_alignment("LEFT"),   pango::Alignment::Left);
        assert_eq!(text_alignment(""),       pango::Alignment::Center);
        assert_eq!(text_alignment("bogus"),  pango::Alignment::Center);
    }

    /// Left vs right alignment must produce different pixel output — the same
    /// centered block, but the text inside is justified differently.
    #[test]
    fn alignment_changes_output() {
        let (w, h) = (900, 400);
        let quote = test_quote();
        let render = |align: pango::Alignment| -> cairo::ImageSurface {
            let mut base = test_theme();
            base.text_alignment = align;
            let mut s = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("s");
            {
                let cr = cairo::Context::new(&s).expect("ctx");
                let mut state = DrawState {
                    quote: &quote,
                    glitch: false, glitch_dx: 0.0, glitch_dy: 0.0,
                    glitch_echo_px: 0.0, glitch_invert: false, glitch_ghost: false,
                    typewriter: quote.text.chars().count(),
                    cursor_on: false, glow: None, text_cache: None,
                };
                draw(&cr, w, h, &mut state, &base);
            }
            s
        };
        let mut left   = render(pango::Alignment::Left);
        let mut center = render(pango::Alignment::Center);
        let mut right  = render(pango::Alignment::Right);

        assert_ne!(
            left.data().expect("surface").to_vec(),
            right.data().expect("surface").to_vec(),
            "left vs right alignment must differ"
        );
        assert_ne!(
            left.data().expect("surface").to_vec(),
            center.data().expect("surface").to_vec(),
            "left vs center alignment must differ"
        );
    }

    /// The author anchors to the quote block's justified edge instead of being
    /// re-centered: its visual left/right edge must coincide with the quote's.
    #[test]
    fn author_anchors_to_justified_quote() {
        let (w, iw, aw) = (900.0, 400.0, 220.0);
        let aink_x = 0.0;

        let left = author_x(pango::Alignment::Left, w, iw, aink_x, aw);
        assert!((left + aink_x - (w / 2.0 - iw / 2.0)).abs() < 1e-9, "author left edge == quote left edge");

        let right = author_x(pango::Alignment::Right, w, iw, aink_x, aw);
        assert!(
            (right + aink_x + aw - (w / 2.0 + iw / 2.0)).abs() < 1e-9,
            "author right edge == quote right edge"
        );

        let center = author_x(pango::Alignment::Center, w, iw, aink_x, aw);
        assert!(
            (center + aink_x + aw / 2.0 - w / 2.0).abs() < 1e-9,
            "author stays centered for center alignment"
        );
    }

    /// The lifetime alpha curve must start at 0 (invisible at birth, where
    /// the squares emerge from the gradient), hold near 1, and return to 0
    /// exactly as the travel progress reaches 1 at the mid-screen target.
    #[test]
    fn square_alpha_curve_is_fade_in_hold_fade_out() {
        assert_eq!(square_alpha(0.0), 0.0);
        assert!((square_alpha(0.09) - 0.5).abs() < 0.001, "linear fade-in at t=0.09");
        assert_eq!(square_alpha(0.18), 1.0);
        assert_eq!(square_alpha(0.5), 1.0);
        assert!((square_alpha(0.85) - 0.5).abs() < 0.001, "linear fade-out at t=0.85");
        assert!(square_alpha(1.0).abs() < 0.001, "alpha must reach ~0 at the target (t=1)");
    }

    /// Speed is proportional to size (the size-parallax), monotonic and
    /// clamped to the lively envelope on both ends.
    #[test]
    fn square_speed_ratio_is_monotonic() {
        assert_eq!(square_speed_ratio(1.0, 12.0), SQUARE_MIN_SPEED_RATIO);
        assert_eq!(square_speed_ratio(100.0, 12.0), SQUARE_MAX_SPEED_RATIO);
        let mut prev = 0.0;
        for size in (0..100).map(|i| 1.0 + i as f64 * 0.35) {
            let r = square_speed_ratio(size, 12.0);
            assert!(r >= prev - 1e-9, "ratio must be non-decreasing ({r} < {prev})");
            prev = r;
        }
        // The reference (medium) size drifts at exactly the base ratio 1.0.
        assert!((square_speed_ratio(12.0, 12.0) - 1.0).abs() < 1e-9);
    }

    /// Spawns fill the whole continuous size range and scale speed with size:
    /// the very small (far-away) fraction must always drift slower than the
    /// very large (close) fraction, regardless of per-particle random jitter.
    #[test]
    fn square_sizes_and_speeds_are_proportional() {
        let theme = test_theme();
        let hi = (theme.font_px * SQUARE_MAX_SCALE).max(SQUARE_MIN_PX + 1.0);
        let span = hi - SQUARE_MIN_PX;
        // In sqrt-bias space, P(size < min + span*q) == q².  0.20 → ~4% of
        // spawns, 0.85 → ~28% of spawns large; speeds are well separated even
        // with the 0.8..1.25 random jitter.
        let small_cut = SQUARE_MIN_PX + span * 0.20;
        let large_cut = SQUARE_MIN_PX + span * 0.85;

        let mut rng = rand::thread_rng();
        let mut small_max = 0.0f64;
        let mut large_min = f64::MAX;
        let (mut n_small, mut n_large) = (0usize, 0usize);
        for _ in 0..600 {
            let p = spawn_square(800.0, 600.0, &theme, &mut rng);
            assert!(
                (SQUARE_MIN_PX..=hi).contains(&p.size),
                "size must stay within the spawn range"
            );
            if p.size < small_cut {
                small_max = small_max.max(p.speed);
                n_small += 1;
            } else if p.size > large_cut {
                large_min = large_min.min(p.speed);
                n_large += 1;
            }
        }
        assert!(
            n_small > 10 && n_large > 10,
            "both size buckets must populate ({n_small}, {n_large})"
        );
        assert!(
            small_max < large_min,
            "small far-away squares must drift slower than big close ones ({small_max} vs {large_min})"
        );
    }

    /// The ticking update advances a particle toward its target, keeps the
    /// dead (t >= 1) ones out, and caps live particles at `squares_max`.
    #[test]
    fn square_update_drifts_and_evicts() {
        let mut theme = test_theme();
        theme.squares_spawn_rate = 0.0; // deterministic: never auto-spawn
        theme.squares_max = u32::MAX;
        let mut field = SquareField::new();
        let mut p = spawn_square(800.0, 600.0, &theme, &mut rand::thread_rng());
        p.glitch_ticks = 0;
        p.next_glitch = u32::MAX; // never glitch during the test
        field.particles.push(p);
        let (w, h) = (800, 600);
        // A handful of ticks must move the particle upward toward its target.
        for _ in 0..10 {
            update_square_field(&mut field, w, h, 0.05, &theme);
        }
        assert_eq!(field.particles.len(), 1);
        assert!(field.particles[0].t > 0.0, "progress must increase");
        assert!(p.y >= field.particles[0].y, "y must drift toward the top");

        // A particle already at the end is evicted on the next tick.
        let mut done = spawn_square(800.0, 600.0, &theme, &mut rand::thread_rng());
        done.t = 1.0;
        done.glitch_ticks = 0;
        done.next_glitch = u32::MAX;
        field.particles.push(done);
        update_square_field(&mut field, w, h, 0.05, &theme);
        assert_eq!(
            field.particles.len(),
            1,
            "expired particle must be culled"
        );

        // Drawing a fully-visible particle paints a solid sized square: fill
        // it at alpha 1 on a surface and probe a pixel inside its bounds.
        field.particles[0].size = 8.0;
        field.particles[0].t = 0.5;
        field.particles[0].max_alpha = 1.0;
        field.particles[0].x = 100.0;
        field.particles[0].y = 300.0;
        reset_glitch(&mut field.particles[0]);
        let mut surf = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
        {
            let cr = cairo::Context::new(&surf).expect("context");
            draw_squares(&cr, &field, &theme);
        }
        let data = surf.data().expect("data");
        // Fill covers x 100..108 / y 300..308; probe the center.
        let pl = (304 * w + 104) as usize * 4;
        let bright = data[pl].max(data[pl + 1]).max(data[pl + 2]);
        assert!(
            bright > 200,
            "fully-visible square must paint bright pixels (got {bright})"
        );
    }

    /// The birth gradient paints a soft bottom band (the square-spawn zone)
    /// and leaves the top of the frame untouched.
    #[test]
    fn birth_gradient_paints_bottom_band() {
        let (w, h) = (400, 400);
        let theme = test_theme();
        let mut surf = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
        {
            let cr = cairo::Context::new(&surf).expect("context");
            cr.set_source_rgb(theme.bg.0, theme.bg.1, theme.bg.2);
            cr.paint();
            draw_birth_gradient(&cr, w as f64, h as f64, &theme);
        }
        let data = surf.data().expect("data");
        let bottom_pl = ((h - 4) * w + w / 2) as usize * 4;
        let top_pl = (10 * w + w / 2) as usize * 4;
        let bottom = data[bottom_pl].max(data[bottom_pl + 1]).max(data[bottom_pl + 2]);
        let top = data[top_pl].max(data[top_pl + 1]).max(data[top_pl + 2]);
        assert!(bottom > top + 30, "bottom band must be brighter than the top ({bottom} vs {top})");
        assert!(top < 30, "top of the frame must stay dark ({top})");
    }
}