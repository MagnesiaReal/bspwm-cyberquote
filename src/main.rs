//! bspwm-cyberquote-native — GTK3 + Cairo/Pango host (no WebView).
//!
//! Spawns one undecorated GTK3 window per monitor.  Each window is stamped with
//! `WindowTypeHint::Desktop` (the GTK3 native way — GTK itself sets
//! `_NET_WM_WINDOW_TYPE_DESKTOP` at map time, exactly like the old webkit2gtk
//! builds) so bspwm skips tiling it and keeps it at the bottom of the stacking
//! order == a wallpaper.  A `DrawingArea` paints the quote ticker via the
//! `render` module (Cairo/Pango): no WebView, no JS.  The text/glow layer is
//! redrawn only on quote changes, glitch bursts, or resize; the only thing
//! that animates continuously is a transparent RGBA overlay rolling the
//! scanlines top→bottom at ~20 fps (cheap: just the scanline bands).
//!
//! `config`/`quotes` are shared verbatim with the original repo, so this binary
//! reads the same `~/.config/bspwm-cyberquote/config.toml` and quote pool.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gtk::gdk;
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow, DrawingArea};

use rand::Rng;

use bspwm_cyberquote::config::{Config, ConfigMonitorInfo, Orientation, WindowGeometry};
use bspwm_cyberquote::quotes::{load_all, load_and_pick_many, Quote};
use bspwm_cyberquote::render;

// ---------------------------------------------------------------------------
// Config path — matches the original and the fork's shared config module.
// ---------------------------------------------------------------------------
const CONFIG_PATH: &str = concat!(env!("HOME"), "/.config/bspwm-cyberquote/config.toml");

/// Precedence: per-user CONFIG_PATH → repo `./config.toml` → defaults.
fn load_cfg() -> Result<Config, (PathBuf, String)> {
    let candidates: [PathBuf; 2] = [PathBuf::from(CONFIG_PATH), PathBuf::from("config.toml")];
    let mut last: Option<(PathBuf, String)> = None;
    for path in candidates {
        match bspwm_cyberquote::config::load_config(&path) {
            Ok(c) => {
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

/// Everything a monitor window owns.  `Rc`/`RefCell` because GTK3 closures
/// (draw, timers) each capture their own clones.
struct MonitorWindow {
    window: ApplicationWindow,
    area: DrawingArea,
    scan_area: DrawingArea,
    quote: Rc<RefCell<Quote>>,
    glitch_on: Rc<Cell<bool>>,
    glitch_dx: Rc<Cell<f64>>,
    glitch_dy: Rc<Cell<f64>>,
    scan_phase: Rc<Cell<f64>>,
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
        cyan: render::hex_color(&cfg.accent.cyan),
        magenta: render::hex_color(&cfg.accent.magenta),
        orange: render::hex_color(&cfg.accent.orange),
        font_family: first_font_family(&cfg.display.font),
        font_px: cfg.display.font_size.max(8.0) as f64,
        scanline_alpha: cfg.accent.scanline_opacity.clamp(0.0, 1.0) as f64,
        scanline_lines: cfg.accent.scanline_lines,
        animated_scanlines: false, // flipped per-window once RGBA is probed
        glitch_intensity: cfg.accent.glitch_intensity.clamp(0.0, 1.0) as f64,
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
    let glitch_ms = (cfg.accent.glitch_duration.max(0.05) * 1000.0) as u64;

    for (idx, geometry) in geometries.iter().enumerate() {
        let mw = build_monitor_window(
            app, &cfg, theme.clone(), geometry, selections[idx].quote.clone(), is_wayland,
        );
        arm_glitch_timer(&mw.area, &mw.glitch_on, &mw.glitch_dx, &mw.glitch_dy, glitch_ms);
        if cycle_minutes > 0 {
            arm_cycle_timer(&mw.area, &mw.quote, &pool, cycle_minutes);
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
    let area_all = area.clone();
    area.connect_draw(move |_, cr| {
        let q = q.borrow();
        let state = render::DrawState {
            quote: &q,
            glitch: g_on.get(),
            glitch_dx: g_dx.get(),
            glitch_dy: g_dy.get(),
        };
        render::draw(cr, area_all.allocated_width(), area_all.allocated_height(), &state, &main_theme);
        glib::Propagation::Proceed
    });

    if animated {
        let s_area = scan_area.clone();
        let s_theme = theme.clone();
        let s_phase = scan_phase.clone();
        scan_area.connect_draw(move |_, cr| {
            // Clear to fully transparent first, then only paint the bands.
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            cr.paint().ok();
            render::draw_scanline_layer(
                cr,
                s_area.allocated_width(),
                s_area.allocated_height(),
                &s_theme,
                s_phase.get(),
            );
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

    MonitorWindow {
        window,
        area,
        scan_area,
        quote,
        glitch_on,
        glitch_dx,
        glitch_dy,
        scan_phase,
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

/// Arm a glitch burst `random(2s..12s)` from now.  On fire: random displacement,
/// queue a redraw, hold the burst for `glitch_ms`, then release and re-arm.
fn arm_glitch_timer(
    area: &DrawingArea,
    glitch_on: &Rc<Cell<bool>>,
    glitch_dx: &Rc<Cell<f64>>,
    glitch_dy: &Rc<Cell<f64>>,
    glitch_ms: u64,
) {
    let area = area.clone();
    let glitch_on = glitch_on.clone();
    let glitch_dx = glitch_dx.clone();
    let glitch_dy = glitch_dy.clone();

    let mut rng = rand::thread_rng();
    let delay_ms = rng.gen_range(2000..=12_000);
    glib::timeout_add_local(std::time::Duration::from_millis(delay_ms), move || {
        let mut rng = rand::thread_rng();
        glitch_dx.set(rng.gen_range(-1.0..=1.0));
        glitch_dy.set(rng.gen_range(-1.0..=1.0));
        glitch_on.set(true);
        area.queue_draw();

        let hold_ms = glitch_ms.max(150);
        let area = area.clone();
        let glitch_on = glitch_on.clone();
        let glitch_dx = glitch_dx.clone();
        let glitch_dy = glitch_dy.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(hold_ms), move || {
            glitch_on.set(false);
            glitch_dx.set(0.0);
            glitch_dy.set(0.0);
            area.queue_draw();
            glib::ControlFlow::Break
        });
        glib::ControlFlow::Continue
    });
}

/// Rotate the quote on this window every `cycle_minutes` minutes (source stays
/// armed forever — `timeout_add_seconds` re-arms automatically).
fn arm_cycle_timer(area: &DrawingArea, quote: &Rc<RefCell<Quote>>, pool: &Rc<Vec<Quote>>, cycle_minutes: u32) {
    let area = area.clone();
    let quote = quote.clone();
    let pool = pool.clone();
    glib::timeout_add_seconds_local((cycle_minutes * 60) as u32, move || {
        let mut rng = rand::thread_rng();
        if !pool.is_empty() {
            let i = rng.gen_range(0..pool.len());
            let mut q = quote.borrow_mut();
            q.text.clone_from(&pool[i].text);
            q.author.clone_from(&pool[i].author);
            area.queue_draw();
        }
        glib::ControlFlow::Continue
    });
}

/// Roll the scanline mesh continuously from the top edge toward the bottom.
///
/// One full screen-height sweep takes `SCAN_SWEEP_SECS` (matches the HTML
/// version's `--scanline-speed: 90s`), redrawn at ~20 fps.  The per-frame work
/// is only the overlay's ~`lines` thin bands — the text/glow layer is not
/// repainted by this timer.
const SCAN_SWEEP_SECS: f64 = 90.0;
const SCAN_ANIM_MS: u64 = 50;

fn arm_scan_timer(area: &DrawingArea, phase: &Rc<Cell<f64>>, h: f64, lines: u32) {
    if lines == 0 || h <= 0.0 {
        return;
    }
    let spacing = (h / lines as f64).max(2.0);
    let delta = h / (SCAN_SWEEP_SECS * (1000.0 / SCAN_ANIM_MS as f64));

    let area = area.clone();
    let phase = phase.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(SCAN_ANIM_MS), move || {
        let p = phase.get() + delta;
        phase.set(if p >= spacing { p - spacing } else { p });
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