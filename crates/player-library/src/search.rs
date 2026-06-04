//! Search helpers: the in-memory nucleo haystack entry plus the top-N fuzzy
//! matcher used for instant typeahead across every entity group.

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::Matcher;

/// One entry in a nucleo haystack: an id plus the text matched against. For the
/// tracks haystack the id is the `track.id`; for the albums/artists/folders
/// haystacks it is the index into the corresponding cached `Vec` (those
/// entities are derived once at index-build time and kept in RAM).
pub(crate) struct Hay {
    pub id: i64,
    pub text: String,
}

impl AsRef<str> for Hay {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

/// Fuzzy-match `query` against `hays` and return the ids of the top `limit`
/// entries by descending score. `matcher` is threaded through so one allocation
/// is reused across all of a query's entity groups. Callers must guard against
/// an empty/whitespace `query` (nucleo would match everything).
pub(crate) fn match_topn(matcher: &mut Matcher, hays: &[Hay], query: &str, limit: usize) -> Vec<i64> {
    let pat = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    pat.match_list(hays.iter(), matcher)
        .into_iter()
        .take(limit)
        .map(|(h, _score)| h.id)
        .collect()
}
