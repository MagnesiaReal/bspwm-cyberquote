//! bspwm-cyberquote-native — GTK3 + Cairo/Pango host (no WebView).
//!
//! Spawns one undecorated GTK3 window per monitor.  Each window is stamped with
//! `WindowTypeHint::Desktop` (the GTK3 native way — GTK itself sets
//! `_NET_WM_WINDOW_TYPE_DESKTOP` at map time, exactly like the old webkit2gtk
//! builds) so bspwm skips tiling it and keeps it at the bottom of the stacking
//! order == a wallpaper.  A `DrawingArea` paints the quote ticker via the
//! `render` module (Cairo/Pango): no WebView, no JS.  Each phrase opens with a
//! one-shot typewriter reveal (~30 ms/char, see `start_typewriter`); when it is
//! done the text layer redraws only on quote changes, glitch bursts, or
//! resize.  The only things that change continuously are a transparent RGBA
//! overlay rolling the scanlines top→bottom at ~20 fps (cheap: just the
//! scanline bands) and the blinking terminal caret riding that same overlay
//! (`arm_cursor_blink`, ~1.9 Hz) — the heavy layer is never repainted for
//! either.
//!
//! `config`/`quotes` are shared verbatim with the original repo, so this binary
//! reads the same `~/.config/bspwm-cyberquote/config.toml` and quote pool.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::gdk;
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow, DrawingArea};

use rand::Rng;

use bspwm_cyberquote::config::{Config, ConfigMonitorInfo, Orientation, WindowGeometry};
use bspwm_cyberquote::quotes::{load_all, load_and_pick_many, Quote};
use bspwm_cyberquote::render;

/// Lazily-built scanline tile for the animated overlay: keyed by (w, h), so
/// it's rebuilt only on resize and reused for the whole 20 fps sweep.
type TileCache = Rc<RefCell<Option<(usize, usize, gtk::cairo::ImageSurface)>>>;

// ---------------------------------------------------------------------------
// Config path — matches the original and the fork's shared config module.
// ---------------------------------------------------------------------------
const CONFIG_PATH: &str = concat!(env!("HOME"), "/.config/bspwm-cyberquote/config.toml");

/// System-wide default, shipped by the Arch package to `/etc`.
const SYS_CONFIG_PATH: &str = "/etc/bspwm-cyberquote/config.toml";

/// Precedence: per-user CONFIG_PATH → `/etc` default → repo `./config.toml` →
/// defaults.  On first run (no user config yet) the `/etc` sample shipped by
/// the package is copied into `~/.config` so the editable file lives there.
fn load_cfg() -> Result<Config, (PathBuf, String)> {
    let candidates: [PathBuf; 3] = [
        PathBuf::from(CONFIG_PATH),
        PathBuf::from(SYS_CONFIG_PATH),
        PathBuf::from("config.toml"),
    ];
    let mut last: Option<(PathBuf, String)> = None;
    for path in candidates {
        match bspwm_cyberquote::config::load_config(&path) {
            Ok(c) => {
                if path == PathBuf::from(SYS_CONFIG_PATH) {
                    seed_user_config(&path);
                }
                eprintln!("bspwm-cyberquote-native: config loaded from {}", path.display());
                return Ok(c);
            }
            Err(e) => last = Some((path, e.to_string())),
        }
    }
    match last {
        Some((p, s)) => Err((p, s)),
        None => Err((PathBuf::from(CONFIG_PATH), "no config candidate loaded".into())),
    }
}

/// First-run convenience: when the only config found is the packaged `/etc`
/// default, copy it to `~/.config/bspwm-cyberquote/config.toml` so the user
/// has an editable copy.  A best-effort — failure is non-fatal.
fn seed_user_config(sys_path: &Path) {
    let user_path = PathBuf::from(CONFIG_PATH);
    if user_path.exists() {
        return;
    }
    if let Some(dir) = user_path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    match std::fs::copy(sys_path, &user_path) {
        Ok(_) => {
            eprintln!(
                "bspwm-cyberquote-native: seeded {} from the system default",
                user_path.display()
            );
        }
        Err(e) => {
            eprintln!(
                "bspwm-cyberquote-native: could not seed {}: {}",
                user_path.display(),
                e
            );
        }
    }
}

/// Everything a monitor window owns.  `Rc`/`RefCell` because GTK3 closures
/// (draw, timers) each capture their own clones.
struct MonitorWindow {
    window: ApplicationWindow,
    area: DrawingArea,
    scan_area: DrawingArea,
    scan_tile: TileCache,
    quote: Rc<RefCell<Quote>>,
    glitch_on: Rc<Cell<bool>>,
    glitch_dx: Rc<Cell<f64>>,
    glitch_dy: Rc<Cell<f64>>,
    glitch_invert: Rc<Cell<bool>>,
    glitch_ghost: Rc<Cell<bool>>,
    glitch_echo_px: Rc<Cell<f64>>,
    scan_phase: Rc<Cell<f64>>,
    /// Typewriter reveal progress (chars of the current quote shown so far).
    typewriter_chars: Rc<Cell<usize>>,
    /// Bumped every time a new phrase starts, so stale reveal timers die.
    typewriter_token: Rc<Cell<u64>>,
    /// Blink phase of the terminal caret (toggled by `arm_cursor_blink`).
    cursor_on: Rc<Cell<bool>>,
}

fn main() {
    let app = Application::builder()
        .application_id("com.bspwm.cyberquote-native")
        .build();

    app.connect_activate(build_windows);

    let _ = app.run();
}

fn build_windows(app: &Application) {
    let cfg = match load_cfg() {
        Ok(c) => c,
        Err((_, e)) => {
            eprintln!("bspwm-cyberquote-native: config load failed ({}), using defaults", e);
            Config::defaults()
        }
    };

    let theme = render::Theme {
        bg: render::hex_color(&cfg.display.background_color),
        fg: render::hex_color(&cfg.display.foreground_color),
        color_a: render::hex_color(&cfg.glitch.color_a),
        color_b: render::hex_color(&cfg.glitch.color_b),
        orange: render::hex_color(&cfg.accent.orange),
        font_family: first_font_family(&cfg.display.font),
        font_px: cfg.display.font_size.max(8.0) as f64,
        author_font_px: {
            let raw = cfg.display.author_font_size;
            (if raw > 0.0 { raw } else { cfg.display.font_size }).max(8.0) as f64
        },
        text_alignment: render::text_alignment(&cfg.display.text_alignment),
        scanline_alpha: cfg.accent.scanline_opacity.clamp(0.0, 1.0) as f64,
        scanline_lines: cfg.accent.scanline_lines,
        animated_scanlines: false, // flipped per-window once RGBA is probed
        glitch_intensity: cfg.glitch.glitch_intensity.clamp(0.0, 1.0) as f64,
        author_ink: render::hex_color(&cfg.accent.author_color),
        cursor_ink: render::hex_color(&cfg.accent.cursor_color),
        glow_color: render::hex_color(&cfg.glow.color),
        glow_intensity: if cfg.glow.enabled {
            cfg.glow.intensity.clamp(0.0, 1.0) as f64
        } else {
            0.0
        },
        glow_radius: cfg.glow.radius.max(0.0) as f64,
        glow_thickness: cfg.glow.thickness.max(0.0) as f64,
    };

    // ---- monitor policy ----
    let display = gdk::Display::default().expect("no default Gdk display");
    let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();

    let mut monitors: Vec<gdk::Monitor> = (0..display.n_monitors())
        .filter_map(|i| display.monitor(i))
        .collect();
    if cfg.monitors.primary_only {
        monitors.retain(|m| m.is_primary());
    }

    let monitor_infos: Vec<ConfigMonitorInfo> = monitors
        .iter()
        .map(|m| {
            let g = m.geometry();
            ConfigMonitorInfo {
                width: g.width(),
                height: g.height(),
                x: g.x(),
                y: g.y(),
                rotation: Orientation::Normal,
            }
        })
        .collect();

    let geometries: Vec<WindowGeometry> = bspwm_cyberquote::config::monitor_layout(&monitor_infos);
    eprintln!(
        "bspwm-cyberquote-native: {} monitor(s) -> {} window(s)",
        monitors.len(),
        geometries.len()
    );

    // ---- quote pool (shared for cycling) + one selection per monitor ----
    let quote_path = std::path::Path::new(&cfg.quotes.source);
    let selections = match load_and_pick_many(Some(quote_path), geometries.len()) {
        Ok(sel) => sel,
        Err(e) => {
            eprintln!("bspwm-cyberquote-native: quote load failed ({}), using fallback", e);
            vec![
                bspwm_cyberquote::quotes::QuoteSelection {
                    quote: Quote {
                        text: "System online — awaiting quote feed.".into(),
                        author: "bspwm-cyberquote-native".into(),
                    },
                };
                geometries.len()
            ]
        }
    };
    let pool = match load_all(Some(quote_path)) {
        Ok(q) => Rc::new(q),
        Err(_) => Rc::new(selections.iter().map(|s| s.quote.clone()).collect()),
    };

    let cycle_minutes = cfg.quotes.cycle_interval_minutes;
    let glitch_ms = (cfg.glitch.glitch_duration.max(0.05) * 1000.0) as u64;
    let interval_min_ms = (cfg.glitch.interval_min_seconds.max(0.0) * 1000.0) as u64;
    let interval_max_ms =
        (cfg.glitch.interval_max_seconds.max(cfg.glitch.interval_min_seconds) * 1000.0) as u64;

    for (idx, geometry) in geometries.iter().enumerate() {
        let mw = build_monitor_window(
            app, &cfg, theme.clone(), geometry, selections[idx].quote.clone(), is_wayland,
        );
        arm_glitch_timer(
            &mw.area,
            &mw.glitch_on,
            &mw.glitch_dx,
            &mw.glitch_dy,
            &mw.glitch_invert,
            &mw.glitch_ghost,
            &mw.glitch_echo_px,
            glitch_ms,
            interval_min_ms,
            interval_max_ms,
            theme.glitch_intensity,
        );
        // One-shot typewriter reveal for the first phrase each monitor sees.
        start_typewriter(&mw.area, &mw.quote, &mw.typewriter_chars, &mw.typewriter_token);
        if cycle_minutes > 0 {
            arm_cycle_timer(
                &mw.area,
                &mw.quote,
                &pool,
                cycle_minutes,
                &mw.typewriter_chars,
                &mw.typewriter_token,
            );
        }
        arm_scan_timer(&mw.scan_area, &mw.scan_phase, geometry.height as f64, cfg.accent.scanline_lines);
        mw.window.show_all();
        let dbg = mw.window.window().map(|w| format!("gdk_vis={}", w.is_visible()));
        eprintln!(
            "bspwm-cyberquote-native: window {} {}x{} is_visible={} gdk={:?}",
            idx,
            geometry.width,
            geometry.height,
            mw.window.is_visible(),
            dbg
        );
    }
}

/// Construct one monitor window wired to the Cairo renderer.
fn build_monitor_window(
    app: &Application,
    cfg: &Config,
    theme: render::Theme,
    geometry: &WindowGeometry,
    quote: Quote,
    is_wayland: bool,
) -> MonitorWindow {
    let window = ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .skip_taskbar_hint(true)
        .skip_pager_hint(true)
        .accept_focus(false)
        .build();

    window.set_default_size(geometry.width, geometry.height);
    window.move_(geometry.x, geometry.y);
    if cfg.display.opacity < 1.0 {
        window.set_opacity(cfg.display.opacity as f64);
    }

    // GTK3's native desktop "wallpaper" mechanism: the WM reads the hint at
    // map time, so there is no realize/show ordering to worry about.  Wayland
    // has no desktop-hint layer — fall back to fullscreen on the monitor.
    if !is_wayland {
        window.set_type_hint(gdk::WindowTypeHint::Desktop);
        window.stick();
    }

    let area = DrawingArea::new();

    // The main paint layer draws the (static) background, text and vignette.
    // The moving scanlines go on a transparent RGBA overlay so animating them
    // at ~20 fps only repaints a few hundred thin rects, not the whole frame.
    let screen = gtk::prelude::WidgetExt::screen(&window).expect("window has a screen");
    let has_rgba = screen.rgba_visual().is_some();

    let scan_area = DrawingArea::new();
    let scan_phase = Rc::new(Cell::new(0.0f64));
    let scan_tile = Rc::new(RefCell::new(None));
    let typewriter_chars = Rc::new(Cell::new(0usize));
    let typewriter_token = Rc::new(Cell::new(0u64));
    let cursor_on = Rc::new(Cell::new(true));
    let animated = has_rgba;
    if animated {
        if let Some(v) = screen.rgba_visual() {
            scan_area.set_visual(Some(&v));
        }
        scan_area.set_app_paintable(true);
    }

    let overlay = gtk::Overlay::new();
    overlay.add(&area);
    overlay.add_overlay(&scan_area);
    window.add(&overlay);

    let quote = Rc::new(RefCell::new(quote));
    let glitch_on = Rc::new(Cell::new(false));
    let glitch_dx = Rc::new(Cell::new(0.0f64));
    let glitch_dy = Rc::new(Cell::new(0.0f64));
    let glitch_invert = Rc::new(Cell::new(false));
    let glitch_ghost = Rc::new(Cell::new(false));
    let glitch_echo_px = Rc::new(Cell::new(0.0f64));
    let glow_cache = Rc::new(RefCell::new(render::GlowCache::new()));
    let text_cache = Rc::new(RefCell::new(render::TextSurfaceCache::new()));

    // Main layer draws its own static scanlines only when there is no RGBA
    // compositor to host the animated overlay (avoids double-darkening).
    let main_theme = render::Theme {
        animated_scanlines: !animated,
        ..theme.clone()
    };

    let q = quote.clone();
    let g_on = glitch_on.clone();
    let g_dx = glitch_dx.clone();
    let g_dy = glitch_dy.clone();
    let g_inv = glitch_invert.clone();
    let g_gh = glitch_ghost.clone();
    let g_echo = glitch_echo_px.clone();
    let typer = typewriter_chars.clone();
    let cursor_on_draw = cursor_on.clone();
    let area_all = area.clone();
    let glow_draw = glow_cache.clone();
    let text_draw = text_cache.clone();
    area.connect_draw(move |_, cr| {
        let q = q.borrow();
        let mut glow = glow_draw.borrow_mut();
        let mut tc = text_draw.borrow_mut();
        let mut state = render::DrawState {
            quote: &q,
            glitch: g_on.get(),
            glitch_dx: g_dx.get(),
            glitch_dy: g_dy.get(),
            glitch_echo_px: g_echo.get(),
            glitch_invert: g_inv.get(),
            glitch_ghost: g_gh.get(),
            typewriter: typer.get(),
            cursor_on: cursor_on_draw.get(),
            glow: Some(&mut glow),
            text_cache: Some(&mut tc),
        };
        render::draw(cr, area_all.allocated_width(), area_all.allocated_height(), &mut state, &main_theme);
        glib::Propagation::Proceed
    });

    if animated {
        let s_area = scan_area.clone();
        let s_theme = theme.clone();
        let s_phase = scan_phase.clone();
        let s_tile = scan_tile.clone();
        let s_cursor = cursor_on.clone();
        let s_glitch = glitch_on.clone();
        let s_quote = quote.clone();
        let s_typer = typewriter_chars.clone();
        scan_area.connect_draw(move |_, cr| {
            let tw = s_area.allocated_width();
            let th = s_area.allocated_height();
            // (Re)build the pattern tile only when the size changed; the tile
            // is a plain alpha strip so per-frame work is a single repeat-paint.
            if s_tile
                .borrow()
                .as_ref()
                .map(|(w, h, _)| *w != tw as usize || *h != th as usize)
                .unwrap_or(true)
            {
                *s_tile.borrow_mut() = render::scanline_tile(&s_theme, tw, th)
                    .map(|t| (tw as usize, th as usize, t));
            }
            if let Some((_, _, tile)) = s_tile.borrow().as_ref() {
                render::draw_scanline_tile(cr, &s_theme, s_phase.get(), tile);
            }
            // The blinking terminal caret rides this overlay: it is a single
            // small rect fill per frame, so its ~2 Hz blink costs nothing on
            // the heavy text layer.  Hidden during a glitch burst.
            if s_cursor.get() && !s_glitch.get() {
                let qb = s_quote.borrow();
                if let Some((cx, cy, cw, ch)) = render::cursor_rect(
                    cr, &s_theme, &qb.text, &qb.author, s_typer.get(), tw as f64, th as f64,
                ) {
                    let (r, g, b) = s_theme.cursor_ink;
                    cr.set_source_rgba(r, g, b, 0.9);
                    cr.rectangle(cx, cy, cw, ch);
                    let _ = cr.fill();
                }
            }
            glib::Propagation::Proceed
        });
    }

    if is_wayland {
        let screen = gdk::Screen::default();
        let m_idx = monitor_index_at(window.position());
        window.connect_map(move |w| match (&screen, m_idx) {
            (Some(s), Some(i)) => w.fullscreen_on_monitor(s, i),
            _ => w.fullscreen(),
        });
    }

    // The caret blinks on the scanline overlay when there is a compositor;
    // otherwise it is drawn on the main layer (gated by `DrawState.cursor_on`).
    arm_cursor_blink(
        &area,
        animated.then_some(&scan_area),
        &cursor_on,
    );

    MonitorWindow {
        window,
        area,
        scan_area,
        scan_tile,
        quote,
        glitch_on,
        glitch_dx,
        glitch_dy,
        glitch_invert,
        glitch_ghost,
        glitch_echo_px,
        scan_phase,
        typewriter_chars,
        typewriter_token,
        cursor_on,
    }
}

/// Find the index of the Gdk monitor whose geometry contains `(x, y)`.
fn monitor_index_at((x, y): (i32, i32)) -> Option<i32> {
    let display = gdk::Display::default()?;
    (0..display.n_monitors()).find(|&i| {
        display.monitor(i).is_some_and(|m| {
            let g = m.geometry();
            g.x() <= x && x < g.x() + g.width() && g.y() <= y && y < g.y() + g.height()
        })
    })
}

/// Arm a glitch burst within `min_ms..=max_ms` from now.  On fire, run a
/// multi-step burst (see `run_glitch_burst`), then re-arm with a fresh random
/// delay in the same configured window.
fn arm_glitch_timer(
    area: &DrawingArea,
    glitch_on: &Rc<Cell<bool>>,
    glitch_dx: &Rc<Cell<f64>>,
    glitch_dy: &Rc<Cell<f64>>,
    glitch_invert: &Rc<Cell<bool>>,
    glitch_ghost: &Rc<Cell<bool>>,
    glitch_echo_px: &Rc<Cell<f64>>,
    glitch_ms: u64,
    interval_min_ms: u64,
    interval_max_ms: u64,
    intensity: f64,
) {
    let area = area.clone();
    let glitch_on = glitch_on.clone();
    let glitch_dx = glitch_dx.clone();
    let glitch_dy = glitch_dy.clone();
    let glitch_invert = glitch_invert.clone();
    let glitch_ghost = glitch_ghost.clone();
    let glitch_echo_px = glitch_echo_px.clone();

    let mut rng = rand::thread_rng();
    let delay_ms = rng.gen_range(interval_min_ms..=interval_max_ms);
    glib::timeout_add_local(std::time::Duration::from_millis(delay_ms), move || {
        run_glitch_burst(
            &area,
            &glitch_on,
            &glitch_dx,
            &glitch_dy,
            &glitch_invert,
            &glitch_ghost,
            &glitch_echo_px,
            glitch_ms,
            interval_min_ms,
            interval_max_ms,
            intensity,
        );
        glib::ControlFlow::Break
    });
}

/// One glitch burst, mirroring the HTML `glitch-full` keyframes: the whole
/// text block swings left/right in wide steps with chromatic color_a/color_b
/// echoes, occasional white invert flashes, and blank strobing frames.  The
/// burst lasts ~`glitch_ms`; the last few steps ease back to the origin so
/// the text settles instead of snapping.
const GLITCH_STEP_MS: u64 = 55;

fn run_glitch_burst(
    area: &DrawingArea,
    glitch_on: &Rc<Cell<bool>>,
    glitch_dx: &Rc<Cell<f64>>,
    glitch_dy: &Rc<Cell<f64>>,
    glitch_invert: &Rc<Cell<bool>>,
    glitch_ghost: &Rc<Cell<bool>>,
    glitch_echo_px: &Rc<Cell<f64>>,
    glitch_ms: u64,
    interval_min_ms: u64,
    interval_max_ms: u64,
    intensity: f64,
) {
    let steps = (glitch_ms / GLITCH_STEP_MS).max(6);
    let half = steps / 2;
    let remaining = Rc::new(Cell::new(steps));

    let area = area.clone();
    let glitch_on = glitch_on.clone();
    let glitch_dx = glitch_dx.clone();
    let glitch_dy = glitch_dy.clone();
    let glitch_invert = glitch_invert.clone();
    let glitch_ghost = glitch_ghost.clone();
    let glitch_echo_px = glitch_echo_px.clone();
    glitch_on.set(true);

    glib::timeout_add_local(std::time::Duration::from_millis(GLITCH_STEP_MS), move || {
        let mut rng = rand::thread_rng();
        let k = remaining.get();
        if k == 0 {
            // burst over — settle and re-arm the next random bomb
            glitch_invert.set(false);
            glitch_ghost.set(false);
            glitch_echo_px.set(0.0);
            glitch_dx.set(0.0);
            glitch_dy.set(0.0);
            glitch_on.set(false);
            area.queue_draw();
            arm_glitch_timer(
                &area,
                &glitch_on,
                &glitch_dx,
                &glitch_dy,
                &glitch_invert,
                &glitch_ghost,
                &glitch_echo_px,
                glitch_ms,
                interval_min_ms,
                interval_max_ms,
                intensity,
            );
            return glib::ControlFlow::Break;
        }
        remaining.set(k - 1);

        // displacement: full swings in the middle, easing near the end so the
        // text settles back to center instead of teleporting.
        let amp = 20.0 + 60.0 * intensity;
        let d = if k <= 3 {
            rng.gen_range(-16.0..=16.0)
        } else {
            let m = amp * rng.gen_range(0.6..=1.0);
            if rng.gen_bool(0.5) { m } else { -m }
        };
        glitch_dx.set(d);
        glitch_dy.set(rng.gen_range(-6.0..=6.0));

        // chromatic echoes most steps, dropped sometimes for variety
        if rng.gen_bool(0.75) {
            glitch_echo_px.set(amp * rng.gen_range(1.0..=1.8));
        } else {
            glitch_echo_px.set(0.0);
        }

        // flicker: blank strobe every 4th step, invert flash every 5th
        if k % 4 == 0 {
            glitch_ghost.set(rng.gen_bool(0.7));
        } else {
            glitch_ghost.set(false);
        }
        if k % 5 == 0 {
            glitch_invert.set(rng.gen_bool(0.6));
        } else {
            glitch_invert.set(false);
        }
        if k == half {
            glitch_invert.set(true);
        }
        area.queue_draw();
        glib::ControlFlow::Continue
    });
}

/// Rotate the quote on this window every `cycle_minutes` minutes (source stays
/// armed forever — `timeout_add_seconds` re-arms automatically).  Each swap
/// restarts the one-shot typewriter reveal for the new phrase.
fn arm_cycle_timer(
    area: &DrawingArea,
    quote: &Rc<RefCell<Quote>>,
    pool: &Rc<Vec<Quote>>,
    cycle_minutes: u32,
    typewriter_chars: &Rc<Cell<usize>>,
    typewriter_token: &Rc<Cell<u64>>,
) {
    let area = area.clone();
    let quote = quote.clone();
    let pool = pool.clone();
    let tw_chars = typewriter_chars.clone();
    let tw_token = typewriter_token.clone();
    glib::timeout_add_seconds_local((cycle_minutes * 60) as u32, move || {
        let mut rng = rand::thread_rng();
        if !pool.is_empty() {
            let i = rng.gen_range(0..pool.len());
            {
                let mut q = quote.borrow_mut();
                q.text.clone_from(&pool[i].text);
                q.author.clone_from(&pool[i].author);
            }
            start_typewriter(&area, &quote, &tw_chars, &tw_token);
            area.queue_draw();
        }
        glib::ControlFlow::Continue
    });
}

/// One char per ~`TYPEWRITER_MS_PER_CHAR` until the whole phrase is revealed —
/// a one-shot animation that never repeats for the same quote.  A generation
/// token lets the next phrase silently retire any stale timer.
const TYPEWRITER_MS_PER_CHAR: u64 = 30;

fn start_typewriter(
    area: &DrawingArea,
    quote: &Rc<RefCell<Quote>>,
    chars: &Rc<Cell<usize>>,
    token: &Rc<Cell<u64>>,
) {
    let total = quote.borrow().text.chars().count();
    let gen = token.get() + 1;
    token.set(gen);
    chars.set(0);

    if total == 0 {
        return;
    }

    let area = area.clone();
    let chars = chars.clone();
    let token = token.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(TYPEWRITER_MS_PER_CHAR), move || {
        if token.get() != gen {
            return glib::ControlFlow::Break; // superseded by a newer phrase
        }
        let n = chars.get() + 1;
        if n >= total {
            chars.set(total);
            area.queue_draw();
            return glib::ControlFlow::Break; // reveal finished — one-shot done
        }
        chars.set(n);
        area.queue_draw();
        glib::ControlFlow::Continue
    });
}

/// Toggle the terminal caret blink phase every `CURSOR_BLINK_MS` (a classic
/// ~1.9 Hz terminal timing).  With an RGBA compositor the caret lives on the
/// scanline overlay, whose repaint this ticker also triggers — the heavy
/// text layer is never redrawn by the blink.  Without a compositor the
/// main layer owns the caret and gets the redraw instead (rare fallback).
const CURSOR_BLINK_MS: u64 = 530;

fn arm_cursor_blink(area: &DrawingArea, overlay: Option<&DrawingArea>, cursor_on: &Rc<Cell<bool>>) {
    let area = area.clone();
    let overlay = overlay.cloned();
    let cursor_on = cursor_on.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(CURSOR_BLINK_MS), move || {
        cursor_on.set(!cursor_on.get());
        match &overlay {
            Some(ov) => ov.queue_draw(),
            None => area.queue_draw(),
        }
        glib::ControlFlow::Continue
    });
}

/// Roll the scanline mesh continuously from the top edge toward the bottom.
/// One full screen-height sweep takes `SCAN_SWEEP_SECS` (matches the HTML
/// version's `--scanline-speed: 90s`), redrawn at ~20 fps.  The per-frame work
/// is only the overlay's ~`lines` thin bands — the text layer is not
/// repainted by this timer.
const SCAN_SWEEP_SECS: f64 = 90.0;
const SCAN_ANIM_MS: u64 = 50;

fn arm_scan_timer(area: &DrawingArea, phase: &Rc<Cell<f64>>, h: f64, lines: u32) {
    if lines == 0 || h <= 0.0 {
        return;
    }
    // Wrap on the integer pattern period so it matches the tile exactly.
    let period = render::scanline_tile_height(h, lines);
    let delta = h / (SCAN_SWEEP_SECS * (1000.0 / SCAN_ANIM_MS as f64));

    let area = area.clone();
    let phase = phase.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(SCAN_ANIM_MS), move || {
        let p = phase.get() + delta;
        phase.set(if p >= period { p - period } else { p });
        area.queue_draw();
        glib::ControlFlow::Continue
    });
}

/// "JetBrains Mono, monospace" → the family name Pango can resolve.
fn first_font_family(spec: &str) -> String {
    spec.split(',')
        .next()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("monospace")
        .to_string()
}