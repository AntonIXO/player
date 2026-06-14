//! The Search page: a debounced search entry + a Tracks/Albums/Artists segmented
//! scope over the background search worker (own DB connection + in-memory fuzzy
//! index), and the grouped results renderer (Artists · Albums · Tracks).

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::{glib, Orientation};
use player_library::{Filter, Library, SearchIndex, SearchResults};

use crate::playback::{enqueue_track, play_album, play_artist, play_list, toggle_loved};
use crate::state::{SharedState, SharedUi};
use crate::ui::library::{open_album_for, open_artist_detail};
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

    // Three scoped searches as a linked segmented control (mirrors the Library
    // header). There is no "All": every query is scoped to a single group, so a
    // Tracks search never matches the album/artist haystacks (see
    // `SearchIndex::query`) — that scoping is the speed win. The bar spans the
    // full width of the entry, each segment taking an equal third.
    let segbar = gtk::Box::new(Orientation::Horizontal, 0);
    segbar.add_css_class("linked");
    segbar.set_hexpand(true);
    segbar.set_margin_top(8);
    let filters = [
        ("Tracks", "view-list-symbolic", Filter::Tracks),
        ("Albums", "media-optical-symbolic", Filter::Albums),
        ("Artists", "avatar-default-symbolic", Filter::Artists),
    ];
    let mut buttons = Vec::new();
    for (label, icon, f) in filters {
        let content = gtk::Box::new(Orientation::Horizontal, 6);
        content.set_halign(gtk::Align::Center);
        content.append(&gtk::Image::from_icon_name(icon));
        content.append(&gtk::Label::new(Some(label)));
        let b = gtk::ToggleButton::new();
        b.set_child(Some(&content));
        b.set_hexpand(true);
        if f == Filter::Albums {
            b.set_active(true);
        }
        segbar.append(&b);
        buttons.push((f, b));
    }

    // Entry + filter bar pinned in a top bar, the results as the scrolling
    // content: an AdwToolbarView tracks the bottom safe-area inset, so when phoc
    // (Phosh) resizes the window for the on-screen keyboard, only the results
    // scroller shrinks — the entry stays put and tappable (no reflow jump).
    let top = gtk::Box::new(Orientation::Vertical, 6);
    top.set_margin_top(8);
    top.set_margin_start(12);
    top.set_margin_end(12);
    top.append(&entry);
    top.append(&segbar);

    let results = gtk::Box::new(Orientation::Vertical, 0);
    results.set_margin_top(8);
    results.set_margin_start(16);
    results.set_margin_end(16);
    results.set_margin_bottom(20);
    let results_scroller = wrap_scroller(&clamp(&results));

    let page = adw::ToolbarView::new();
    page.set_top_bar_style(adw::ToolbarStyle::Flat);
    page.add_top_bar(&top);
    page.set_content(Some(&results_scroller));

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

/// Render the grouped results (Artists · Albums · Tracks). With the scoped
/// segmented control only one group is non-empty per query.
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
                None,
                Some(("media-playback-start-symbolic", "Play album")),
                {
                    // Activate → album detail (drill-down); tracks fetched on the worker.
                    let (state, ui, alc) = (state.clone(), ui.clone(), al.clone());
                    move || open_album_for(&state, &ui, &alc.album, alc.album_artist.as_deref())
                },
                {
                    // Trailing ▶ → play the album immediately (fetched on the worker).
                    let (state, ui, alc) = (state.clone(), ui.clone(), al.clone());
                    move || play_album(&state, &ui, &alc.album, alc.album_artist.as_deref())
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
            let (id, loved) = (t.id, t.loved);
            let (state, ui, all) = (state.clone(), ui.clone(), tracks_rc.clone());
            let (s2, u2, tclone) = (state.clone(), ui.clone(), t.clone());
            let (sh, uh) = (state.clone(), ui.clone());
            let cache = ui.art.clone();
            let row = row_widget(
                &cache,
                t.art_hash.as_deref(),
                &t.display_title(),
                &t.subtitle(),
                Some(&t.format_spec()),
                Some((loved, Box::new(move |now| toggle_loved(&sh, &uh, id, now)))),
                Some(("list-add-symbolic", "Add to queue")),
                move || play_list(&state, &ui, (*all).clone(), i),
                move || enqueue_track(&s2, &u2, tclone.clone()),
            );
            lb.append(&row);
        }
        ui.search_results.append(&lb);
    }
}
