//! Search helpers: the FTS5 MATCH-expression builder (with filter-chip column
//! narrowing) and the in-memory nucleo haystack used for instant typeahead.

use crate::model::Filter;

/// One entry in the nucleo haystack: the track id plus the text matched against.
pub(crate) struct Hay {
    pub id: i64,
    pub text: String,
}

impl AsRef<str> for Hay {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

/// Build an FTS5 MATCH expression from a raw query and a filter chip. Each term
/// becomes a prefix token (`dar*`); terms are implicitly ANDed. Returns `None`
/// when the query has no usable tokens. Punctuation is stripped so user input
/// can never form invalid FTS syntax.
pub(crate) fn fts_expr(query: &str, filter: Filter) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(sanitize_term)
        .filter(|t| !t.is_empty())
        .map(|t| format!("{t}*"))
        .collect();
    if terms.is_empty() {
        return None;
    }
    let body = terms.join(" ");
    let col = match filter {
        Filter::All => return Some(body),
        Filter::Albums => "album",
        Filter::Artists => "artist",
        Filter::AlbumArtists => "album_artist",
        Filter::Composers => "composer",
        Filter::Genres => "genre",
    };
    // `{col} : (a* b*)` restricts every term to that column.
    Some(format!("{{{col}}} : ({body})"))
}

/// Keep only alphanumerics (Unicode-aware); the `unicode61 remove_diacritics`
/// tokenizer folds accents on the index side so this need not.
fn sanitize_term(t: &str) -> String {
    t.chars().filter(|c| c.is_alphanumeric()).collect()
}
