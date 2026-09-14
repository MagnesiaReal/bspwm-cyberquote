use serde::Deserialize;
use std::fs;
use std::path::Path;

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

fn default_quotes_path() -> &'static Path {
    // Project-root quotes.json; a config can override this.
    Path::new("quotes.json")
}

/// Load all non-empty quotes from a `quotes.json` file.
pub fn load_all(quotes_path: Option<&Path>) -> Result<Vec<Quote>, String> {
    let path: &Path = quotes_path.unwrap_or_else(|| default_quotes_path());

    let raw = fs::read_to_string(path).map_err(|e| {
        format!("Failed to read {}: {}", path.display(), e)
    })?;

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
