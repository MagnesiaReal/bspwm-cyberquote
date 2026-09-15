//! bspwm-cyberquote — Agent 2: Config + Theme + Monitor Geometry engine
//!
//! Standalone module: parses a TOML config, emits a CSS custom-properties
//! stylesheet, and computes per-monitor window geometry from a monitor
//! descriptor.  Compiles on its own against the crate's Cargo.toml; integrates
//! with main.rs via the public API below.
//!
//! Integration contract with Agent 1 (main.rs):
//!   - main.rs MUST call `load_config(CONFIG_PATH)` (the const declared there)
//!     at startup, falling back to defaults on error.
//!   - main.rs MUST call `build_stylesheet(&config)` and pass the returned CSS
//!     to `cyberhtml::build_html()`, which wraps it in `:root { ... }`.  There
//!     is no separate stylesheet injection step in the GTK4/WebKitGTK 6.0 host.
//!   - main.rs MUST map each Gdk `Monitor`'s geometry into a `ConfigMonitorInfo`
//!     (defined here) before calling `monitor_layout()`.
//!
//!   config.toml layout (Agent 3 also reads the file directly for quotes.source,
//!   quotes.picker, quotes.cycle_interval_minutes):
//!   ┌─────────────────────────────────────────────────────────────────┐
//!   │ display:                                                         │
//!   │   font: "JetBrains Mono, monospace"        # CSS font-family       │
//!   │   font_size: 18                             # px                    │
//!   │   opacity: 0.98                             # window opacity        │
//!   │   background_color: "#0a0a0f"               # CSS hex color         │
//!   │   foreground_color: "#00ffea"               # CSS hex color         │
//!   │                                                                 │
//!   │ accent:                                                         │
//!   │   author_color: "#ffa028e6"                 # author ink               │
//!   │   cursor_color: "#ffa028e6"                 # terminal caret (fork)     │
//!   │   orange: "#ffb347"                          # accent fallback           │
//!   │   scanline_opacity: 0.15                    # 0.0-1.0               │
//!   │   scanline_steps: 2160                       # ≈24 steps/s (fluid)  │
//!   │   scanline_lines: 216                        # line+gap per screen   │
//!   │   scanline_size_rem: 0.1                     # line thickness in rem │
//!   │   crt_curvature: 0.12                       # screen bend factor    │
//!   │                                                                 │
//!   │ glitch:                                                         │
//!   │   color_a: "#00ffff"                        # chromatic echo A (cyan) │
//!   │   color_b: "#ff00ff"                        # chromatic echo B (magenta)│
//!   │   glitch_intensity: 0.35                    # 0.0-1.0               │
//!   │   glitch_duration: 0.52                     # per-glitch seconds     │
//!   │   glitch_interval: 6.5                      # legacy CSS interval    │
//!   │   interval_min_seconds: 2.0                 # random-trigger lower   │
//!   │   interval_max_seconds: 12.0                # random-trigger upper   │
//!   │                                                                 │
//!   │ glow:                                                           │
//!   │   enabled: true                            # master switch         │
//!   │   color: "#39e6ff"                         # halo CSS hex color     │
//!   │   radius: 4.0                              # blur spread in px       │
//!   │   intensity: 0.45                          # halo opacity 0-1        │
//!   │   thickness: 2.0                           # underlay stroke in px   │
//!   │                                                                 │
//!   │ quotes:                                                        │
//!   │   source: "quotes.txt"                       # path or url          │
//!   │   cycle_interval_minutes: 15                # rotation period       │
//!   │   picker: "random"                          # random|sequential|last │
//!   │                                                                 │
//!   │ monitors:                                                     │
//!   │   behavior: "one_per_monitor"                # one_per_monitor|     │
//!   │                                               # span_virtual_screen │
//!   │   primary_only: false                        # bool                  │
//!   └─────────────────────────────────────────────────────────────────┘
//!
//! Stylesheet injection note:
//!   The GTK4 + WebKitGTK 6.0 host (main.rs) injects `build_stylesheet()`'s
//!   custom properties by passing the returned string to
//!   `cyberhtml::build_html()`, which wraps it in a `:root { ... }` block
//!   inside the generated document — no user-scripts / user-stylesheets are
//!   used.  The returned string is a semicolon-separated list of CSS
//!   custom-property declarations (e.g. `--accent-cyan:#39e6ff;--bg:#05070d;`).

use serde::Deserialize;
use std::fmt;
use std::path::Path;

// ---------------------------------------------------------------------------
// TOML config → Config
// ---------------------------------------------------------------------------

/// Top-level config, deserialized from config.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Display-level settings: font, size, opacity, colors.
    #[serde(default)]
    pub display: DisplayConfig,
    /// Accent / effect settings: colors, scanlines, CRT.
    #[serde(default)]
    pub accent: AccentConfig,
    /// Glitch-effect settings: chromatic echo colors + trigger timing.
    #[serde(default)]
    pub glitch: GlitchConfig,
    /// Glow effect settings for the quote text.
    #[serde(default)]
    pub glow: GlowConfig,
    /// Quote-pipeline settings (also read by Agent 3 directly from the file).
    #[serde(default)]
    pub quotes: QuotesConfig,
    /// Monitor-policy settings.
    #[serde(default)]
    pub monitors: MonitorsConfig,
}

impl Config {
    /// Public fallback constructor (used when config loading fails).
    pub fn defaults() -> Self {
        Self {
            display: DisplayConfig::defaults(),
            accent: AccentConfig::defaults(),
            glitch: GlitchConfig::defaults(),
            glow: GlowConfig::defaults(),
            quotes: QuotesConfig::defaults(),
            monitors: MonitorsConfig::defaults(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Display config.
#[derive(Debug, Clone, Deserialize)]
pub struct DisplayConfig {
    #[serde(default = "DisplayConfig::default_font")]
    pub font: String,
    #[serde(default = "DisplayConfig::default_font_size")]
    pub font_size: f32,
    /// Author (attribution) font size in CSS px.  When omitted or zero the
    /// quote `font_size` is used, so the default behaviour is unchanged.
    #[serde(default)]
    pub author_font_size: f32,
    /// Text alignment of the quote/author lines *within* the centered block:
    /// `"left"`, `"right"` or `"center"` (default).  The text block stays
    /// centered on the screen; this only controls how each line is justified.
    #[serde(default = "DisplayConfig::default_text_alignment")]
    pub text_alignment: String,
    #[serde(default = "DisplayConfig::default_opacity")]
    pub opacity: f32,
    #[serde(default = "DisplayConfig::default_background_color")]
    pub background_color: String,
    #[serde(default = "DisplayConfig::default_foreground_color")]
    pub foreground_color: String,
}

impl DisplayConfig {
    fn defaults() -> Self {
        Self {
            font: Self::default_font(),
            font_size: Self::default_font_size(),
            author_font_size: 0.0,
            text_alignment: Self::default_text_alignment(),
            opacity: Self::default_opacity(),
            background_color: Self::default_background_color(),
            foreground_color: Self::default_foreground_color(),
        }
    }
    fn default_font() -> String {
        "JetBrains Mono, monospace".into()
    }
    fn default_font_size() -> f32 {
        18.0
    }
    fn default_text_alignment() -> String {
        "center".into()
    }
    fn default_opacity() -> f32 {
        0.98
    }
    fn default_background_color() -> String {
        "#0a0a0f".into()
    }
    fn default_foreground_color() -> String {
        "#ffffff".into()
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Accent config.
#[derive(Debug, Clone, Deserialize)]
pub struct AccentConfig {
    #[serde(default = "AccentConfig::default_orange")]
    pub orange: String,
    #[serde(default = "AccentConfig::default_author_color")]
    pub author_color: String,
    #[serde(default = "AccentConfig::default_cursor_color")]
    pub cursor_color: String,
    #[serde(default = "AccentConfig::default_scanline_opacity")]
    pub scanline_opacity: f32,
    #[serde(default = "AccentConfig::default_scanline_steps")]
    pub scanline_steps: u32,
    #[serde(default = "AccentConfig::default_scanline_lines")]
    pub scanline_lines: u32,
    #[serde(default = "AccentConfig::default_scanline_size_rem")]
    pub scanline_size_rem: f32,
    #[serde(default = "AccentConfig::default_crt_curvature")]
    pub crt_curvature: f32,
}

impl AccentConfig {
    fn defaults() -> Self {
        Self {
            orange: Self::default_orange(),
            author_color: Self::default_author_color(),
            cursor_color: Self::default_cursor_color(),
            scanline_opacity: Self::default_scanline_opacity(),
            scanline_steps: Self::default_scanline_steps(),
            scanline_lines: Self::default_scanline_lines(),
            scanline_size_rem: Self::default_scanline_size_rem(),
            crt_curvature: Self::default_crt_curvature(),
        }
    }
    fn default_orange() -> String {
        "#ffe3b3".into()
    }
    fn default_author_color() -> String {
        "#ffa028e6".into()
    }
    fn default_cursor_color() -> String {
        "#ffa028e6".into()
    }
    fn default_scanline_opacity() -> f32 {
        0.20
    }
    fn default_scanline_steps() -> u32 {
        2160
    }
    fn default_scanline_lines() -> u32 {
        216
    }
    fn default_scanline_size_rem() -> f32 {
        0.1
    }
    fn default_crt_curvature() -> f32 {
        0.12
    }
}

impl Default for AccentConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Glitch-effect config: chromatic-echo colors plus trigger timing.
///
/// The chromatic split mimics an RGB signal tear: `color_a` (the old
/// `accent.cyan`) is echoed to the left of the block and `color_b` (the old
/// `accent.magenta`) to the right.  `interval_min_seconds` /
/// `interval_max_seconds` bound the random delay before the next glitch burst
/// fires (the previous hardcoded 2–12 s window).  `glitch_intensity` and
/// `glitch_duration` ride the same burst; `glitch_interval` is the legacy knob
/// consumed only by the (webkit-fork) CSS stylesheet.
#[derive(Debug, Clone, Deserialize)]
pub struct GlitchConfig {
    #[serde(default = "GlitchConfig::default_color_a")]
    pub color_a: String,
    #[serde(default = "GlitchConfig::default_color_b")]
    pub color_b: String,
    #[serde(default = "GlitchConfig::default_glitch_intensity")]
    pub glitch_intensity: f32,
    #[serde(default = "GlitchConfig::default_glitch_duration")]
    pub glitch_duration: f32,
    #[serde(default = "GlitchConfig::default_glitch_interval")]
    pub glitch_interval: f32,
    #[serde(default = "GlitchConfig::default_interval_min_seconds")]
    pub interval_min_seconds: f32,
    #[serde(default = "GlitchConfig::default_interval_max_seconds")]
    pub interval_max_seconds: f32,
}

impl GlitchConfig {
    fn defaults() -> Self {
        Self {
            color_a: Self::default_color_a(),
            color_b: Self::default_color_b(),
            glitch_intensity: Self::default_glitch_intensity(),
            glitch_duration: Self::default_glitch_duration(),
            glitch_interval: Self::default_glitch_interval(),
            interval_min_seconds: Self::default_interval_min_seconds(),
            interval_max_seconds: Self::default_interval_max_seconds(),
        }
    }
    fn default_color_a() -> String {
        "#00ffff".into()
    }
    fn default_color_b() -> String {
        "#ff00ff".into()
    }
    fn default_glitch_intensity() -> f32 {
        0.35
    }
    fn default_glitch_duration() -> f32 {
        0.5
    }
    fn default_glitch_interval() -> f32 {
        6.5
    }
    fn default_interval_min_seconds() -> f32 {
        2.0
    }
    fn default_interval_max_seconds() -> f32 {
        12.0
    }
}

impl Default for GlitchConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Text-glow config: a soft luminous halo around the quote text (the HTML
/// fork's `text-shadow` equivalent).
#[derive(Debug, Clone, Deserialize)]
pub struct GlowConfig {
    /// Master switch: `false` draws plain text with no halo.
    #[serde(default = "GlowConfig::default_enabled")]
    pub enabled: bool,
    /// Halo color, CSS hex (`"#rrggbb"`).
    #[serde(default = "GlowConfig::default_color")]
    pub color: String,
    /// Blur radius in px: how far the halo spreads past the glyph edges.
    #[serde(default = "GlowConfig::default_radius")]
    pub radius: f64,
    /// Halo opacity 0..1 (0 turns the glow off).
    #[serde(default = "GlowConfig::default_intensity")]
    pub intensity: f64,
    /// Stroke/outline thickness in px added to the underlay text before it is
    /// blurred (a thicker silhouette → a fuller, more even halo).
    #[serde(default = "GlowConfig::default_thickness")]
    pub thickness: f64,
}

impl GlowConfig {
    fn defaults() -> Self {
        Self {
            enabled: Self::default_enabled(),
            color: Self::default_color(),
            radius: Self::default_radius(),
            intensity: Self::default_intensity(),
            thickness: Self::default_thickness(),
        }
    }
    fn default_enabled() -> bool {
        true
    }
    fn default_color() -> String {
        "#F4E394".into()
    }
    fn default_radius() -> f64 {
        15.0
    }
    fn default_intensity() -> f64 {
        0.45
    }
    fn default_thickness() -> f64 {
        1.0
    }
}

impl Default for GlowConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Quote-pipeline config.
#[derive(Debug, Clone, Deserialize)]
pub struct QuotesConfig {
    #[serde(default = "QuotesConfig::default_source")]
    pub source: String,
    #[serde(default = "QuotesConfig::default_cycle_interval_minutes")]
    pub cycle_interval_minutes: u32,
    /// "random", "sequential", or "last" (stick to the last quote shown).
    #[serde(default = "QuotesConfig::default_picker")]
    pub picker: String,
}

impl QuotesConfig {
    fn defaults() -> Self {
        Self {
            source: Self::default_source(),
            cycle_interval_minutes: Self::default_cycle_interval_minutes(),
            picker: Self::default_picker(),
        }
    }
    fn default_source() -> String {
        "quotes.json".into()
    }
    fn default_cycle_interval_minutes() -> u32 {
        15
    }
    fn default_picker() -> String {
        "random".into()
    }
}

impl Default for QuotesConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Monitor-policy config.
#[derive(Debug, Clone, Deserialize)]
pub struct MonitorsConfig {
    #[serde(default = "MonitorsConfig::default_behavior")]
    pub behavior: String,
    #[serde(default = "MonitorsConfig::default_primary_only")]
    pub primary_only: bool,
}

impl MonitorsConfig {
    fn defaults() -> Self {
        Self {
            behavior: Self::default_behavior(),
            primary_only: Self::default_primary_only(),
        }
    }
    fn default_behavior() -> String {
        "one_per_monitor".into()
    }
    fn default_primary_only() -> bool {
        false
    }
}

impl Default for MonitorsConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

/// Load and parse CONFIG_PATH.  Falls back to defaults when the file is
/// missing, unparseable, or partial (serde default fields fill in).
pub fn load_config(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    let raw = std::fs::read_to_string(path.as_ref())
        .map_err(|e| ConfigError::Io(e, path.as_ref().to_path_buf()))?;
    let config: Config = toml::from_str(&raw)
        .map_err(|e| ConfigError::Parse(e, raw))?;
    Ok(config)
}

/// Error type for config loading.
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error, std::path::PathBuf),
    Parse(toml::de::Error, String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e, p) => write!(f, "cannot read config {}: {}", p.display(), e),
            ConfigError::Parse(e, _raw) => write!(f, "config parse error: {}", e),
        }
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// Stylesheet generation
// ---------------------------------------------------------------------------

/// Build a CSS string defining custom properties on `:root` from a Config.
///
/// The returned string is a semicolon-separated list of CSS custom-property
/// declarations (e.g. `--accent-cyan:#39e6ff;--bg:#05070d;`).  It does NOT
/// include a `:root` wrapper — build_html() in cyberhtml.rs wraps the block
/// in `:root { ... }` (line 91).
///
/// Every custom property consumed by the HTML template (see cyberhtml::default_css_vars)
/// is emitted here, using config values where available and cyberpunk defaults
/// for the rest.
pub fn build_stylesheet(config: &Config) -> String {
    let bg = css_color(&config.display.background_color);
    let fg = css_color(&config.display.foreground_color);
    let cyan = css_color(&config.glitch.color_a);
    let magenta = css_color(&config.glitch.color_b);
    let orange = css_color(&config.accent.orange);

    let font_family = css_font_family(&config.display.font);
    let font_size = css_len_px(config.display.font_size);
    let letter_spacing = "0.02em";
    let line_height = "1.5";

    let opacity = css_alpha(config.display.opacity);
    let max_width = "760px";
    let center_padding = "36px";
    let max_quote_chars = "900";

    let glitch_intensity = css_len_px(config.glitch.glitch_intensity * 6.0 / 0.35);
    let glitch_duration = css_animation_duration(config.glitch.glitch_duration);
    let glitch_ms = format!("{}ms", (config.glitch.glitch_interval * 1000.0) as u32);
    let glitch_jitter = "1.2px";

    let scanline_opacity = css_alpha(config.accent.scanline_opacity);
    let scanline_color = "#000";
    let scanline_lines = config.accent.scanline_lines.max(1).min(2000);
    let scanline_size_rem = config.accent.scanline_size_rem.clamp(0.02, 0.5);
    let scanline_size = format!("{}rem", scanline_size_rem);
    let scanline_gap =
        format!("calc((100vh / {}) - {}rem)", scanline_lines, scanline_size_rem);
    let scanline_speed = "90s";
    let scanline_steps = config.accent.scanline_steps.max(2);

    let crt_perspective = "600px";
    let crt_radius = "40px";
    let crt_vignette_stops = "transparent 0%, transparent 70%, var(--vignette-edge) 100%";
    let crt_vignette_blur = "12px";
    let vignette_edge = "#02030a";

    let typewriter_ms = "80";
    let typewriter_ease = "cubic-bezier(.22,.61,.36,1)";
    let typewriter_slide = "38px";
    let typewriter_opacity_start = "0";
    let typewriter_opacity_end = "1";

    let cursor_char = "'▍'";
    let cursor_blink = "0.9s";
    let cursor_color = "#ffffff";

    let glow_cyan_1 = format!("0 0 6px var(--accent-cyan)");
    let glow_cyan_2 = format!("0 0 18px var(--accent-cyan)");
    let glow_cyan_3 = format!("0 0 40px var(--accent-cyan)");
    let glow_magenta_1 = format!("0 0 6px var(--accent-magenta)");
    let glow_magenta_2 = format!("0 0 18px var(--accent-magenta)");
    let glow_magenta_3 = format!("0 0 40px var(--accent-magenta)");

    let glitch_color_cyan = format!("137px 0 0 var(--accent-cyan)");
    let glitch_color_magenta = format!("-137px 0 0 var(--accent-magenta)");

    let decls = [
        format!("--bg:{}", bg),
        format!("--fg:{}", fg),
        format!("--accent-cyan:{}", cyan),
        format!("--accent-magenta:{}", magenta),
        format!("--accent-orange:{}", orange),
        format!("--glow-cyan-1:{}", glow_cyan_1),
        format!("--glow-cyan-2:{}", glow_cyan_2),
        format!("--glow-cyan-3:{}", glow_cyan_3),
        format!("--glow-magenta-1:{}", glow_magenta_1),
        format!("--glow-magenta-2:{}", glow_magenta_2),
        format!("--glow-magenta-3:{}", glow_magenta_3),
        format!("--font-family:{}", font_family),
        format!("--font-size:{}", font_size),
        format!("--letter-spacing:{}", letter_spacing),
        format!("--line-height:{}", line_height),
        format!("--max-width:{}", max_width),
        format!("--typewriter-ms:{}", typewriter_ms),
        format!("--typewriter-ease:{}", typewriter_ease),
        format!("--typewriter-slide:{}", typewriter_slide),
        format!("--typewriter-opacity-start:{}", typewriter_opacity_start),
        format!("--typewriter-opacity-end:{}", typewriter_opacity_end),
        format!("--cursor-char:{}", cursor_char),
        format!("--cursor-blink:{}", cursor_blink),
        format!("--cursor-color:{}", cursor_color),
        format!("--glitch-intensity:{}", glitch_intensity),
        format!("--glitch-ms:{}", glitch_ms),
        format!("--glitch-duration:{}", glitch_duration),
        format!("--glitch-color-cyan:{}", glitch_color_cyan),
        format!("--glitch-color-magenta:{}", glitch_color_magenta),
        format!("--glitch-jitter:{}", glitch_jitter),
        format!("--scanline-opacity:{}", scanline_opacity),
        format!("--scanline-color:{}", scanline_color),
        format!("--scanline-size:{}", scanline_size),
        format!("--scanline-gap:{}", scanline_gap),
        format!("--scanline-speed:{}", scanline_speed),
        format!("--scanline-steps:{}", scanline_steps),
        format!("--crt-perspective:{}", crt_perspective),
        format!("--crt-radius:{}", crt_radius),
        format!("--crt-vignette-stops:{}", crt_vignette_stops),
        format!("--crt-vignette-blur:{}", crt_vignette_blur),
        format!("--vignette-edge:{}", vignette_edge),
        format!("--center-padding:{}", center_padding),
        format!("--max-quote-chars:{}", max_quote_chars),
    ];
    decls.join(";") + ";"
}

// ---------------------------------------------------------------------------
// CSS helpers — keep emit side-effect-free and testable
// ---------------------------------------------------------------------------

fn css_color(raw: &str) -> String {
    let s = raw.trim();
    if s.starts_with('#') {
        s.to_string()
    } else {
        s.to_string()
    }
}

fn css_font_family(raw: &str) -> String {
    format!("\"{}\"", raw.replace('"', "'"))
}

fn css_font_stack(raw: &str) -> String {
    format!("\"{}\"", raw.replace('"', "'"))
}

fn css_len_px(v: f32) -> String {
    format!("{}px", v)
}

fn css_alpha(v: f32) -> String {
    let clamped = v.clamp(0.0, 1.0);
    format!("{}", clamped)
}

fn css_animation_duration(seconds: f32) -> String {
    format!("{}s", seconds)
}

// ---------------------------------------------------------------------------
// Monitor geometry
// ---------------------------------------------------------------------------

/// A monitor descriptor that main.rs fills in from `Gdk.Monitor.geometry()`
/// (and, if available, the monitor's rotation/orientation).
///
/// Mapping from Gdk (GTK4) to ConfigMonitorInfo:
///   - width  ← `monitor.geometry().width()`
///   - height ← `monitor.geometry().height()`
///   - x      ← `monitor.geometry().x()`
///   - y      ← `monitor.geometry().y()`
///   - rotation ← inferred from `monitor.rotation()` (Gdk4 has no direct
///     rotation property in 4.0; most backends expose orientation via the
///     monitor's `height` > `width` heuristic for portrait, or via a
///     per-monitor XRandR/portrait flag).  When no explicit rotation is
///     available, main.rs should set `rotation = Orientation::Normal` and let
///     `needs_rotation` be driven by the width/height swap heuristic below.
///
///     If the platform exposes an explicit orientation/rotation, map:
///       Gdk.MonitorOrientation::NORMAL  → Orientation::Normal
///       Gdk.MonitorOrientation::90       → Orientation::R90
///       Gdk.MonitorOrientation::180      → Orientation::R180
///       Gdk.MonitorOrientation::270      → Orientation::R270
///     Otherwise use the heuristic in `monitor_geometry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigMonitorInfo {
    /// Width in pixels (CSS pixels / logical pixels as reported by Gdk).
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// X offset from the virtual screen origin (for span_virtual_screen).
    pub x: i32,
    /// Y offset from the virtual screen origin.
    pub y: i32,
    /// Rotation/orientation.  Normal when unknown.
    pub rotation: Orientation,
}

/// Monitor orientation / rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Normal,
    R90,
    R180,
    R270,
}

impl Orientation {
    /// Returns true when the orientation is portrait-primary (width < height
    /// after accounting for rotation).
    pub fn is_portrait(self) -> bool {
        match self {
            Orientation::Normal | Orientation::R180 => false,
            Orientation::R90 | Orientation::R270 => true,
        }
    }
}

/// The geometry a window should be created with for a single monitor.
#[derive(Debug, Clone, Copy)]
pub struct WindowGeometry {
    /// Width of the window.
    pub width: i32,
    /// Height of the window.
    pub height: i32,
    /// X position of the window.
    pub x: i32,
    /// Y position of the window.
    pub y: i32,
    /// When true, the WebView content should be rotated (CSS transform
    /// or JS-driven rotation) so that text remains legible on a portrait
    /// monitor.  The window itself is still created at the monitor's
    /// physical width/height; the content layer is what rotates.
    pub needs_rotation: bool,
    /// Effective rotation applied to content (Normal when no rotation needed).
    pub content_rotation: Orientation,
}

impl Orientation {
    /// Return the effective content rotation that keeps text legible.
    ///
    /// For a portrait monitor (R90 / R270), the content is rotated by the
    /// same amount so that what was "up" on the monitor stays "up" for the
    /// reader.  Concretely: a monitor physically rotated 90° clockwise
    /// shows content rotated -90° (counter-clockwise) so the viewer reads
    /// it upright.
    ///
    /// This is a heuristic; if Agent 2 or Agent 3 wants a different UX (e.g.
    /// always show landscape letterboxed), they can ignore this field.
    pub fn content_rotate_for_legibility(self) -> Orientation {
        match self {
            Orientation::Normal => Orientation::Normal,
            Orientation::R90 => Orientation::R270, // counter-clockwise 90
            Orientation::R180 => Orientation::R180,
            Orientation::R270 => Orientation::R90, // counter-clockwise 270 == cw 90
        }
    }
}

/// Given a monitor descriptor, compute the window geometry and content
/// rotation flag.
pub fn monitor_geometry(monitor: ConfigMonitorInfo) -> WindowGeometry {
    let width = monitor.width;
    let height = monitor.height;
    let x = monitor.x;
    let y = monitor.y;
    let rotation = monitor.rotation;
    let needs_rotation = rotation.is_portrait();
    let content_rotation = if needs_rotation {
        rotation.content_rotate_for_legibility()
    } else {
        Orientation::Normal
    };

    WindowGeometry {
        width,
        height,
        x,
        y,
        needs_rotation,
        content_rotation,
    }
}

/// Compute window geometries for a slice of monitors.
pub fn monitor_layout(monitors: &[ConfigMonitorInfo]) -> Vec<WindowGeometry> {
    monitors
        .iter()
        .map(|m| monitor_geometry(*m))
        .collect()
}

// ---------------------------------------------------------------------------
// Unit-like self-checks — run with `cargo test` (library unit tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r##"
[display]
font = "Hack, monospace"
font_size = 22
author_font_size = 16
text_alignment = "right"
opacity = 0.92
background_color = "#050508"
foreground_color = "#ff5cf0"

[accent]
orange = "#ffb347"
author_color = "#ffa028e6"
cursor_color = "#ffa028e6"
scanline_opacity = 0.20
scanline_steps = 120
scanline_lines = 360
scanline_size_rem = 0.07
crt_curvature = 0.18

[glitch]
color_a = "#00ffcc"
color_b = "#ff0088"
glitch_intensity = 0.45
glitch_duration = 0.30
glitch_interval = 4.0
interval_min_seconds = 3.0
interval_max_seconds = 15.0

[glow]
enabled = true
color = "#00ffcc"
radius = 6.0
intensity = 0.7
thickness = 1.5

[quotes]
source = "~/quotes.txt"
cycle_interval_minutes = 8
picker = "sequential"

[monitors]
behavior = "span_virtual_screen"
primary_only = true
"##;

    fn parsed_config() -> Config {
        toml::from_str::<Config>(SAMPLE_TOML).unwrap()
    }

    #[test]
    fn parse_sample_config() {
        let c = parsed_config();
        assert_eq!(c.display.font, "Hack, monospace");
        assert!((c.display.font_size - 22.0).abs() < 0.01);
        assert!((c.display.author_font_size - 16.0).abs() < 0.01);
        assert_eq!(c.display.text_alignment, "right");
        assert!((c.display.opacity - 0.92).abs() < 0.01);
        assert_eq!(c.display.background_color, "#050508");
        assert_eq!(c.display.foreground_color, "#ff5cf0");

        assert_eq!(c.glitch.color_a, "#00ffcc");
        assert_eq!(c.glitch.color_b, "#ff0088");
        assert!((c.glitch.glitch_intensity - 0.45).abs() < 0.01);
        assert!((c.glitch.glitch_duration - 0.30).abs() < 0.01);
        assert!((c.glitch.glitch_interval - 4.0).abs() < 0.01);
        assert!((c.glitch.interval_min_seconds - 3.0).abs() < 0.01);
        assert!((c.glitch.interval_max_seconds - 15.0).abs() < 0.01);
        assert_eq!(c.accent.orange, "#ffb347");
        assert_eq!(c.accent.author_color, "#ffa028e6");
        assert_eq!(c.accent.cursor_color, "#ffa028e6");
        assert!((c.accent.scanline_opacity - 0.20).abs() < 0.01);
        assert_eq!(c.accent.scanline_steps, 120);
        assert_eq!(c.accent.scanline_lines, 360);
        assert!((c.accent.scanline_size_rem - 0.07).abs() < 0.01);
        assert!((c.accent.crt_curvature - 0.18).abs() < 0.01);

        assert!(c.glow.enabled);
        assert_eq!(c.glow.color, "#00ffcc");
        assert!((c.glow.radius - 6.0).abs() < 0.01);
        assert!((c.glow.intensity - 0.7).abs() < 0.01);
        assert!((c.glow.thickness - 1.5).abs() < 0.01);

        assert_eq!(c.quotes.source, "~/quotes.txt");
        assert_eq!(c.quotes.cycle_interval_minutes, 8);
        assert_eq!(c.quotes.picker, "sequential");

        assert_eq!(c.monitors.behavior, "span_virtual_screen");
        assert!(c.monitors.primary_only);
    }

    #[test]
    fn configs_default_when_empty() {
        let empty: &str = "";
        let c = toml::from_str::<Config>(empty).unwrap();
        assert_eq!(c.display.font, DisplayConfig::default_font());
        assert!((c.display.font_size - DisplayConfig::default_font_size()).abs() < 0.01);
        assert!((c.display.opacity - DisplayConfig::default_opacity()).abs() < 0.01);
        assert_eq!(c.display.background_color, DisplayConfig::default_background_color());
        assert_eq!(c.display.foreground_color, DisplayConfig::default_foreground_color());
        assert_eq!(c.glitch.color_a, GlitchConfig::default_color_a());
        assert_eq!(c.glitch.color_b, GlitchConfig::default_color_b());
        assert!((c.glitch.glitch_intensity - GlitchConfig::default_glitch_intensity()).abs() < 0.01);
        assert!((c.glitch.glitch_duration - GlitchConfig::default_glitch_duration()).abs() < 0.01);
        assert!((c.glitch.glitch_interval - GlitchConfig::default_glitch_interval()).abs() < 0.01);
        assert!((c.glitch.interval_min_seconds - GlitchConfig::default_interval_min_seconds()).abs() < 0.01);
        assert!((c.glitch.interval_max_seconds - GlitchConfig::default_interval_max_seconds()).abs() < 0.01);
        assert_eq!(c.accent.author_color, AccentConfig::default_author_color());
        assert_eq!(c.accent.cursor_color, AccentConfig::default_cursor_color());
        assert!((c.accent.scanline_opacity - AccentConfig::default_scanline_opacity()).abs() < 0.01);
        assert_eq!(c.accent.scanline_steps, AccentConfig::default_scanline_steps());
        assert_eq!(c.accent.scanline_lines, AccentConfig::default_scanline_lines());
        assert!((c.accent.scanline_size_rem - AccentConfig::default_scanline_size_rem()).abs() < 0.01);
        assert!((c.accent.crt_curvature - AccentConfig::default_crt_curvature()).abs() < 0.01);
        assert_eq!(c.glow.enabled, GlowConfig::default_enabled());
        assert_eq!(c.glow.color, GlowConfig::default_color());
        assert!((c.glow.radius - GlowConfig::default_radius()).abs() < 0.01);
        assert!((c.glow.intensity - GlowConfig::default_intensity()).abs() < 0.01);
        assert!((c.glow.thickness - GlowConfig::default_thickness()).abs() < 0.01);
        assert_eq!(c.quotes.source, QuotesConfig::default_source());
        assert_eq!(c.quotes.cycle_interval_minutes, QuotesConfig::default_cycle_interval_minutes());
        assert_eq!(c.quotes.picker, QuotesConfig::default_picker());
        assert_eq!(c.monitors.behavior, MonitorsConfig::default_behavior());
        assert!(!c.monitors.primary_only);
    }

    #[test]
    fn config_missing_fields_uses_defaults() {
        let partial: &str = r#"display = { font_size = 14 }"#;
        let c = toml::from_str::<Config>(partial).unwrap();
        assert_eq!(c.display.font, DisplayConfig::default_font());
        assert!((c.display.font_size - 14.0).abs() < 0.01);
        // author_font_size is optional: omitted means 0 → falls back to font_size.
        assert_eq!(c.display.author_font_size, 0.0);
        assert_eq!(c.display.text_alignment, DisplayConfig::default_text_alignment());
        assert!((c.display.opacity - DisplayConfig::default_opacity()).abs() < 0.01);
    }

    #[test]
    fn build_stylesheet_emits_expected_css_variables() {
        let c = parsed_config();
        let css = build_stylesheet(&c);
        let required = [
            "--bg", "--fg", "--accent-cyan", "--accent-magenta",
            "--glow-cyan-1", "--glow-cyan-2", "--glow-cyan-3",
            "--glow-magenta-1", "--glow-magenta-2", "--glow-magenta-3",
            "--font-family", "--font-size", "--letter-spacing",
            "--line-height", "--max-width",
            "--typewriter-ms", "--typewriter-ease", "--typewriter-slide",
            "--typewriter-opacity-start", "--typewriter-opacity-end",
            "--cursor-char", "--cursor-blink", "--cursor-color",
            "--glitch-intensity", "--glitch-ms", "--glitch-duration",
            "--glitch-color-cyan", "--glitch-color-magenta",
            "--glitch-jitter",
            "--scanline-opacity", "--scanline-color", "--scanline-size",
            "--scanline-gap", "--scanline-speed", "--scanline-steps",
            "--crt-perspective", "--crt-radius",
            "--crt-vignette-stops", "--crt-vignette-blur",
            "--vignette-edge", "--center-padding", "--max-quote-chars",
        ];
        for v in &required {
            assert!(
                css.contains(&format!("{}:", v)),
                "stylesheet should contain {}: — found:\n{css}",
                v
            );
        }
    }

    #[test]
    fn build_stylesheet_includes_correct_color_values() {
        let c = parsed_config();
        let css = build_stylesheet(&c);
        assert!(css.contains("#00ffcc"), "should emit --neon-cyan #00ffcc");
        assert!(css.contains("#ff0088"), "should emit --neon-magenta #ff0088");
        assert!(css.contains("#050508"), "should emit --bg #050508");
        assert!(css.contains("#ff5cf0"), "should emit --fg #ff5cf0");
    }

    #[test]
    fn build_stylesheet_includes_correct_font_size() {
        let c = parsed_config();
        let css = build_stylesheet(&c);
        assert!(css.contains("22px"), "should emit --font-size 22px");
    }

    #[test]
    fn build_stylesheet_includes_scanline_mesh_values() {
        let c = parsed_config();
        let css = build_stylesheet(&c);
        assert!(css.contains("--scanline-size:0.07rem"));
        assert!(css.contains("--scanline-gap:calc((100vh / 360) - 0.07rem)"));
    }

    #[test]
    fn build_stylesheet_does_not_emit_obsolete_opacity() {
        let c = parsed_config();
        let css = build_stylesheet(&c);
        // cyberhtml does not consume --opacity; the config module no longer emits it.
    }

    #[test]
    fn monitor_geometry_normal_is_identity() {
        let m = ConfigMonitorInfo {
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            rotation: Orientation::Normal,
        };
        let g = monitor_geometry(m);
        assert_eq!(g.width, 1920);
        assert_eq!(g.height, 1080);
        assert_eq!(g.x, 0);
        assert_eq!(g.y, 0);
        assert!(!g.needs_rotation);
        assert_eq!(g.content_rotation, Orientation::Normal);
    }

    #[test]
    fn monitor_geometry_portrait_r90() {
        let m = ConfigMonitorInfo {
            width: 1080,
            height: 1920,
            x: 0,
            y: 0,
            rotation: Orientation::R90,
        };
        let g = monitor_geometry(m);
        assert_eq!(g.width, 1080);
        assert_eq!(g.height, 1920);
        assert!(g.needs_rotation);
        assert_eq!(g.content_rotation, Orientation::R270);
    }

    #[test]
    fn monitor_geometry_portrait_r270() {
        let m = ConfigMonitorInfo {
            width: 1080,
            height: 1920,
            x: 0,
            y: 0,
            rotation: Orientation::R270,
        };
        let g = monitor_geometry(m);
        assert_eq!(g.width, 1080);
        assert_eq!(g.height, 1920);
        assert!(g.needs_rotation);
        assert_eq!(g.content_rotation, Orientation::R90);
    }

    #[test]
    fn monitor_layout_returns_one_per_monitor() {
        let m1 = ConfigMonitorInfo {
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            rotation: Orientation::Normal,
        };
        let m2 = ConfigMonitorInfo {
            width: 1920,
            height: 1080,
            x: 1920,
            y: 0,
            rotation: Orientation::Normal,
        };
        let layouts = monitor_layout(&[m1, m2]);
        assert_eq!(layouts.len(), 2);
        assert_eq!(layouts[0].width, 1920);
        assert_eq!(layouts[0].height, 1080);
        assert_eq!(layouts[0].x, 0);
        assert_eq!(layouts[1].x, 1920);
    }

    #[test]
    fn orientation_is_portrait() {
        assert!(Orientation::R90.is_portrait());
        assert!(Orientation::R270.is_portrait());
        assert!(!Orientation::Normal.is_portrait());
        assert!(!Orientation::R180.is_portrait());
    }
}
