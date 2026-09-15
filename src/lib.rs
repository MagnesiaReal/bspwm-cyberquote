//! bspwm-cyberquote — Cairo/Pango desktop quote ticker for bspwm.
//!
//! No WebView: `main.rs` (the GTK3 host) places one Desktop-typed window per
//! monitor and `render` paints the quote ticker — scanlines, vignette,
//! RGB-split glitch, terminal cursor — directly with Cairo.
//!
//! `config` and `quotes` provide the config schema and quote pool
//! (`quotes.json` is a real file in this repo).  `CONFIG_PATH` points at
//! `~/.config/bspwm-cyberquote/config.toml`.

pub mod config;
pub mod quotes;
pub mod render;