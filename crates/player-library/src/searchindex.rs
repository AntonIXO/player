//! In-memory fuzzy search index, built off the `track` table and served
//! separately from [`Library`] (it is `Send`, so a worker thread can own it).
//! Split out of `lib.rs`; reuses the exact browse queries in [`crate::browse`].

use nucleo::{Config, Matcher};

use crate::browse::{
    album_order, load_albums, load_artists, load_folders, load_tracks_by_ids, load_tracks_sorted,
    load_track_hays, track_order,
};
use crate::model::{Album, Artist, Filter, Folder, SearchResults, Sort};
use crate::search::{self, Hay};
use crate::{Library, Result};

/// In-memory fuzzy search index. Three nucleo haystacks (tracks, albums,
/// artists — plus a secondary folders one) built once from the `track` table,
/// so the album/artist/folder groupings (a `GROUP BY` each) run at build time
/// rather than per keystroke. Cheap to query (all matching is in RAM; only the
/// matched track rows are fetched back from the DB), and `Send`, so it can live
/// on a search worker thread.
pub struct SearchIndex {
    track_hays: Vec<Hay>,
    albums: Vec<Album>,
    album_hays: Vec<Hay>,
    artists: Vec<Artist>,
    artist_hays: Vec<Hay>,
    folders: Vec<Folder>,
    folder_hays: Vec<Hay>,
}

/// Index each item into a nucleo haystack entry, keyed by its position (the
/// position maps back into the parallel `Vec<T>` on a hit).
fn hays_from<T>(items: &[T], text: impl Fn(&T) -> String) -> Vec<Hay> {
    items
        .iter()
        .enumerate()
        .map(|(i, x)| Hay {
            id: i as i64,
            text: text(x),
        })
        .collect()
}

impl SearchIndex {
    /// Build the index from a library's current database contents. Rebuild
    /// after a scan to pick up changes.
    pub fn build(lib: &Library) -> Result<Self> {
        let conn = &lib.conn;
        let track_hays = load_track_hays(conn)?;

        let albums = load_albums(conn, album_order(Sort::Album))?;
        let album_hays = hays_from(&albums, |a| match a.album_artist.as_deref() {
            Some(aa) if !aa.is_empty() => format!("{} {}", a.album, aa),
            _ => a.album.clone(),
        });

        let artists = load_artists(conn)?;
        let artist_hays = hays_from(&artists, |a| a.name.clone());

        let folders = load_folders(conn)?;
        let folder_hays = hays_from(&folders, |f| f.path.clone());

        Ok(Self {
            track_hays,
            albums,
            album_hays,
            artists,
            artist_hays,
            folders,
            folder_hays,
        })
    }

    /// Fuzzy search, scoped by `filter`. A specific filter (Tracks/Albums/Artists)
    /// matches *only* that group's haystack — the GTK shell always sends one scope
    /// (its segmented control has no "All"), so an Albums search never pays to match
    /// the tracks/artists haystacks. [`Filter::All`] (still used by the CLI and the
    /// tests) fills artists, albums and tracks plus the secondary folders group from
    /// one call. `limit` caps each group. An empty query returns the first `limit`
    /// of each in-scope group (title order for tracks). `lib` is used only to fetch
    /// the matched track rows.
    pub fn query(
        &self,
        lib: &Library,
        query: &str,
        filter: Filter,
        limit: usize,
    ) -> Result<SearchResults> {
        let conn = &lib.conn;
        let q = query.trim();
        let want = |f: Filter| filter == Filter::All || filter == f;
        let mut out = SearchResults::default();

        if q.is_empty() {
            if want(Filter::Tracks) {
                out.tracks = load_tracks_sorted(conn, track_order(Sort::Title), Some(limit))?;
            }
            if want(Filter::Albums) {
                out.albums = self.albums.iter().take(limit).cloned().collect();
            }
            if want(Filter::Artists) {
                out.artists = self.artists.iter().take(limit).cloned().collect();
            }
            if filter == Filter::All {
                out.folders = self.folders.iter().take(limit).cloned().collect();
            }
            return Ok(out);
        }

        // One matcher, reused across every group of this query.
        let mut matcher = Matcher::new(Config::DEFAULT);
        if want(Filter::Tracks) {
            let ids = search::match_topn(&mut matcher, &self.track_hays, q, limit);
            out.tracks = load_tracks_by_ids(conn, &ids)?;
        }
        if want(Filter::Albums) {
            out.albums = search::match_topn(&mut matcher, &self.album_hays, q, limit)
                .into_iter()
                .filter_map(|i| self.albums.get(i as usize).cloned())
                .collect();
        }
        if want(Filter::Artists) {
            out.artists = search::match_topn(&mut matcher, &self.artist_hays, q, limit)
                .into_iter()
                .filter_map(|i| self.artists.get(i as usize).cloned())
                .collect();
        }
        if filter == Filter::All {
            out.folders = search::match_topn(&mut matcher, &self.folder_hays, q, limit)
                .into_iter()
                .filter_map(|i| self.folders.get(i as usize).cloned())
                .collect();
        }
        Ok(out)
    }
}
