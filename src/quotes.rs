use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// A single quote with its attributed source.
#[derive(Debug, Clone, Deserialize)]
pub struct Quote {
    pub text: String,
    pub author: String,
}

impl Quote {
    /// Trivial validity check used by the loader's filter.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}
#[derive(Debug, Clone, Deserialize)]
pub struct QuotesFile {
    #[serde(default)]
    pub quotes: Vec<Quote>,
}

/// How the loader picks a quote.
#[derive(Debug, Clone, Copy, Deserialize)]
pub enum Picker {
    Random,
}

impl Default for Picker {
    fn default() -> Self {
        Picker::Random
    }
}

/// The state the quote engine produces for the rest of the app.
#[derive(Debug, Clone)]
pub struct QuoteSelection {
    pub quote: Quote,
}

/// The user-level config directory: `$XDG_CONFIG_HOME/bspwm-cyberquote`, or
/// `~/.config/bspwm-cyberquote` when XDG_CONFIG_HOME is unset.
fn user_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("bspwm-cyberquote"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/bspwm-cyberquote"))
}

/// Default search locations for the quote pool, in order:
/// `~/.config/bspwm-cyberquote/quotes.json`, then
/// `/etc/bspwm-cyberquote/quotes.json` (the packaged fallback).
fn default_quotes_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(dir) = user_config_dir() {
        v.push(dir.join("quotes.json"));
    }
    v.push(PathBuf::from("/etc/bspwm-cyberquote/quotes.json"));
    v
}

/// Candidate quote files to try, in order.  An explicitly configured `source`
/// is tried first and honoured when it exists.  For a relative source (the
/// default `"quotes.json"`) the working-directory attempt is followed by the
/// user and global locations; an absolute override is authoritative on its own.
fn candidate_paths(quotes_path: Option<&Path>) -> Vec<PathBuf> {
    match quotes_path {
        Some(p) => {
            let mut v = vec![p.to_path_buf()];
            if p.is_relative() {
                let name = p
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("quotes.json"));
                if let Some(dir) = user_config_dir() {
                    v.push(dir.join(&name));
                }
                v.push(PathBuf::from("/etc/bspwm-cyberquote").join(&name));
            }
            v
        }
        None => default_quotes_paths(),
    }
}

/// Read and validate one quotes file.
fn read_quotes(path: &Path) -> Result<Vec<Quote>, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;

    let file: QuotesFile = serde_json::from_str(&raw).map_err(|e| {
        format!("Failed to parse {}: {}", path.display(), e)
    })?;

    let quotes = file
        .quotes
        .into_iter()
        .filter(|q| !q.text.trim().is_empty())
        .collect::<Vec<_>>();

    if quotes.is_empty() {
        return Err(format!("{} contained no usable quotes.", path.display()));
    }

    Ok(quotes)
}

/// Load all non-empty quotes from the first readable quotes file.
///
/// Search order: a configured `source` first (honoured when it exists);
/// otherwise (or for a relative source that is missing) the defaults are
/// tried in turn — `~/.config/bspwm-cyberquote/quotes.json`, then the
/// packaged `/etc/bspwm-cyberquote/quotes.json`.
pub fn load_all(quotes_path: Option<&Path>) -> Result<Vec<Quote>, String> {
    let candidates = candidate_paths(quotes_path);
    let mut tried = Vec::new();
    for path in &candidates {
        match read_quotes(path) {
            Ok(quotes) => return Ok(quotes),
            Err(e) => tried.push(e),
        }
    }
    Err(format!(
        "No readable quotes file (tried {}): {}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        tried.join("; ")
    ))
}

/// Pick a single quote using `picker`. Random is the default picker.
pub fn load_and_pick(
    quotes_path: Option<&Path>,
    picker: Picker,
) -> Result<QuoteSelection, String> {
    let quotes = load_all(quotes_path)?;

    let chosen = match picker {
        Picker::Random => {
            let mut rng = rand::thread_rng();
            rand::Rng::gen_range(&mut rng, 0..quotes.len())
        }
    };

    Ok(QuoteSelection {
        quote: quotes[chosen].clone(),
    })
}

/// Pick `count` distinct random quotes — one per monitor.
///
/// If the pool has fewer entries than `count`, the pool is shuffled and then
/// cycled so every monitor still receives a quote (duplicates allowed when
/// necessary).
pub fn load_and_pick_many(
    quotes_path: Option<&Path>,
    count: usize,
) -> Result<Vec<QuoteSelection>, String> {
    let mut quotes = load_all(quotes_path)?;

    let mut rng = rand::thread_rng();
    for i in (1..quotes.len()).rev() {
        let j = rand::Rng::gen_range(&mut rng, 0..=i);
        quotes.swap(i, j);
    }

    let mut out = Vec::with_capacity(count);
    for idx in 0..count {
        let quote = quotes[idx % quotes.len()].clone();
        out.push(QuoteSelection { quote });
    }
    Ok(out)
}
