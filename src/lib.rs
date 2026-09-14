//! bspwm-cyberquote-native — Cairo/Pango fork of bspwm-cyberquote.
//!
//! Drops the WebKitGTK WebView entirely: `main.rs` (the GTK3 host) places one
//! Desktop-typed window per monitor and `render` paints the quote ticker —
//! scanlines, vignette, RGB-split glitch, terminal cursor — directly with Cairo.
//!
//! `config` and `quotes` are copied verbatim from the original repo so both apps
//! share the same config schema and quote pool (`quotes.json` is a symlink back
//! to the original).  `CONFIG_PATH` also matches, so both apps read the same
//! `~/.config/bspwm-cyberquote/config.toml`.

pub mod config;
pub mod quotes;
pub mod render;