//! The Search page: a debounced search entry + filter chips over the background
//! search worker (own DB connection + in-memory fuzzy index), and the grouped
//! results renderer (Artists · Albums · Tracks · Folders).

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::{glib, Orientation};
use player_library::{Filter, Library, SearchIndex, SearchResults};

use crate::playback::{enqueue_track, play_artist, play_folder, play_list};
use crate::state::{SharedState, SharedUi};
use crate::ui::library::{build_album_detail, open_artist_detail};
use crate::widgets::{boxed_list, clamp, row_widget, section_label, wrap_scroller};

/// A request to / result from the background search worker.
pub(crate) enum SearchMsg {
    Query { seq: u64, text: String, filter: Filter },
    /// Rebuild the in-memory fuzzy index (after a scan changed the library).
    Reindex,
}

pub(crate) struct SearchHits {
    pub(crate) seq: u64,
    pub(crate) results: SearchResults,
}

pub(crate) fn build_search(
) -> (gtk::Widget, gtk::SearchEntry, gtk::Box, Vec<(Filter, gtk::ToggleButton)>) {
    let entry = gtk::SearchEntry::new();
    entry.set_placeholder_text(Some("Search your library"));
    entry.set_hexpand(true);

    let chips_inner = gtk::Box::new(Orientation::Horizontal, 8);
    let filters = [
        ("All", Filter::All),
        ("Tracks", Filter::Tracks),
        ("Albums", Filter::Albums),
        ("Artists", Filter::Artists),
    ];
    let mut buttons = Vec::new();
    for (label, f) in filters {
        let b = gtk::ToggleButton::with_label(label);
        b.add_css_class("pill");
        if f == Filter::All {
            b.set_active(true);
        }
        chips_inner.append(&b);
        buttons.push((f, b));
    }
    let chips = gtk::ScrolledWindow::new();
    chips.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
    chips.set_child(Some(&chips_inner));
    chips.set_margin_top(8);

    let top = gtk::Box::new(Orientation::Vertical, 0);
    top.set_margin_top(10);
    top.set_margin_start(16);
    top.set_margin_end(16);
    top.append(&entry);
    top.append(&chips);

    let results = gtk::Box::new(Orientation::Vertical, 0);
    results.set_margin_top(8);
    results.set_margin_start(16);
    results.set_margin_end(16);
    results.set_margin_bottom(20);
    let results_scroller = wrap_scroller(&clamp(&results));

    let page = gtk::Box::new(Orientation::Vertical, 0);
    page.append(&top);
    page.append(&results_scroller);

    (page.upcast(), entry, results, buttons)
}

/// Spawn the search worker: it owns its own read-only library connection and an
/// in-memory [`SearchIndex`], answers `Query` messages off the main thread, and
/// rebuilds the index on `Reindex`. Bursts that pile up while it is busy are
/// coalesced to the latest query (older keystrokes never render — latest-wins).
pub(crate) fn spawn_search_worker(
    db: PathBuf,
    art: PathBuf,
) -> (async_channel::Sender<SearchMsg>, async_channel::Receiver<SearchHits>) {
    let (qtx, qrx) = async_channel::unbounded::<SearchMsg>();
    let (rtx, rrx) = async_channel::unbounded::<SearchHits>();
    std::thread::spawn(move || {
        let Ok(lib) = Library::open(&db, &art) else { return };
        let mut index = SearchIndex::build(&lib).ok();
        while let Ok(first) = qrx.recv_blocking() {
            // Coalesce the burst that piled up while we were busy: keep only the
            // latest query and honour any Reindex.
            let mut latest: Option<(u64, String, Filter)> = None;
            let mut reindex = false;
            let mut pending = Some(first);
            while let Some(m) = pending.take().or_else(|| qrx.try_recv().ok()) {
                match m {
                    SearchMsg::Query { seq, text, filter } => latest = Some((seq, text, filter)),
                    SearchMsg::Reindex => reindex = true,
                }
            }
            if reindex {
                index = SearchIndex::build(&lib).ok();
            }
            if let (Some((seq, text, filter)), Some(idx)) = (latest, index.as_ref()) {
                if let Ok(results) = idx.query(&lib, &text, filter, 50) {
                    let _ = rtx.send_blocking(SearchHits { seq, results });
                }
            }
        }
    });
    (qtx, rrx)
}

/// Handle a search-entry change or filter toggle: bump the generation (dropping
/// any in-flight result), and — after a short debounce — hand the query to the
/// worker. An empty query just clears the results.
pub(crate) fn kick_search(ui: &SharedUi) {
    let text = ui.search_entry.text().to_string();
    let seq = ui.search_seq.get().wrapping_add(1);
    ui.search_seq.set(seq);
    if text.trim().is_empty() {
        clear_search_results(ui);
        return;
    }
    let filter = *ui.filter.borrow();
    let ui = ui.clone();
    glib::timeout_add_local_once(Duration::from_millis(80), move || {
        if ui.search_seq.get() != seq {
            return; // a newer keystroke superseded this one before it was sent
        }
        let _ = ui.search_tx.try_send(SearchMsg::Query { seq, text, filter });
    });
}

pub(crate) fn clear_search_results(ui: &SharedUi) {
    while let Some(c) = ui.search_results.first_child() {
        ui.search_results.remove(&c);
    }
}

/// Render the unified grouped results (Artists · Albums · Tracks · Folders).
pub(crate) fn render_search_results(state: &SharedState, ui: &SharedUi, results: SearchResults) {
    clear_search_results(ui);

    if !results.artists.is_empty() {
        ui.search_results.append(&section_label("Artists"));
        let lb = boxed_list();
        for a in &results.artists {
            let meta = format!(
                "{} album{} · {} track{}",
                a.album_count,
                if a.album_count == 1 { "" } else { "s" },
                a.track_count,
                if a.track_count == 1 { "" } else { "s" },
            );
            let (s1, u1, n1) = (state.clone(), ui.clone(), a.name.clone());
            let (s2, u2, n2) = (state.clone(), ui.clone(), a.name.clone());
            let row = row_widget(
                &ui.art,
                None,
                &a.name,
                &meta,
                None,
                Some(("media-playback-start-symbolic", "Play all")),
                move || open_artist_detail(&s1, &u1, &n1),
                move || play_artist(&s2, &u2, &n2),
            );
            lb.append(&row);
        }
        ui.search_results.append(&lb);
    }

    if !results.albums.is_empty() {
        ui.search_results.append(&section_label("Albums"));
        let lb = boxed_list();
        for al in &results.albums {
            let row = row_widget(
                &ui.art,
                al.art_hash.as_deref(),
                &al.album,
                al.album_artist.as_deref().unwrap_or("Unknown Artist"),
                Some(&al.meta()),
                Some(("media-playback-start-symbolic", "Play album")),
                {
                    // Activate → album detail (drill-down).
                    let (state, ui, alc) = (state.clone(), ui.clone(), al.clone());
                    move || {
                        let tracks = state
                            .borrow()
                            .library
                            .album_tracks(&alc.album, alc.album_artist.as_deref())
                            .unwrap_or_default();
                        let page = build_album_detail(&state, &ui, &alc, tracks);
                        ui.nav.push(&page);
                        ui.stack.set_visible_child_name("library");
                    }
                },
                {
                    // Trailing ▶ → play the album immediately.
                    let (state, ui, alc) = (state.clone(), ui.clone(), al.clone());
                    move || {
                        let tracks = state
                            .borrow()
                            .library
                            .album_tracks(&alc.album, alc.album_artist.as_deref())
                            .unwrap_or_default();
                        play_list(&state, &ui, tracks, 0);
                    }
                },
            );
            lb.append(&row);
        }
        ui.search_results.append(&lb);
    }

    if !results.tracks.is_empty() {
        ui.search_results.append(&section_label("Tracks"));
        let lb = boxed_list();
        let tracks_rc = Rc::new(results.tracks.clone());
        for (i, t) in results.tracks.iter().enumerate() {
            let (state, ui, all) = (state.clone(), ui.clone(), tracks_rc.clone());
            let (s2, u2, tclone) = (state.clone(), ui.clone(), t.clone());
            let cache = ui.art.clone();
            let row = row_widget(
                &cache,
                t.art_hash.as_deref(),
                &t.display_title(),
                &t.subtitle(),
                Some(&t.format_spec()),
                Some(("list-add-symbolic", "Add to queue")),
                move || play_list(&state, &ui, (*all).clone(), i),
                move || enqueue_track(&s2, &u2, tclone.clone()),
            );
            lb.append(&row);
        }
        ui.search_results.append(&lb);
    }

    if !results.folders.is_empty() {
        ui.search_results.append(&section_label("Folders"));
        let lb = boxed_list();
        for f in &results.folders {
            let cache = ui.art.clone();
            let (state, ui, path) = (state.clone(), ui.clone(), f.path.clone());
            let row = row_widget(
                &cache,
                None,
                f.name(),
                &f.path,
                Some(&f.meta()),
                Some(("media-playback-start-symbolic", "Play folder")),
                {
                    let (state, ui, path) = (state.clone(), ui.clone(), path.clone());
                    move || play_folder(&state, &ui, &path)
                },
                move || play_folder(&state, &ui, &path),
            );
            lb.append(&row);
        }
        ui.search_results.append(&lb);
    }
}
