//! Renderers for the virtualised browse-list tabs (Artists / Folders / Tracks /
//! Loved). Each builds a `ListView` from a worker-fetched row list and installs
//! it into its scroller; [`super::show_browse_tab`] drives them.

use gtk4 as gtk;

use player_library::{Artist, Folder, Track};

use super::detail::open_artist_detail;
use crate::list::list_view;
use crate::playback::{enqueue_track, play_artist, play_folder, play_list, toggle_loved};
use crate::state::{SharedState, SharedUi};
use crate::widgets::{row_setup, status_page, RowHandle};

/// Build and install the Artists virtualised list from a fetched artist list.
pub(super) fn render_artists(state: &SharedState, ui: &SharedUi, artists: Vec<Artist>) {
    let lv = list_view(
        artists,
        row_setup,
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |h: &RowHandle, a: &Artist| {
                let meta = format!(
                    "{} album{} · {} track{}",
                    a.album_count,
                    if a.album_count == 1 { "" } else { "s" },
                    a.track_count,
                    if a.track_count == 1 { "" } else { "s" },
                );
                h.set_art(None, &ui.art);
                h.set_texts(&a.name, &meta, None);
                h.set_heart(None);
                let (state, ui, name) = (state.clone(), ui.clone(), a.name.clone());
                h.set_trailing(Some((
                    "media-playback-start-symbolic",
                    "Play all",
                    Box::new(move || play_artist(&state, &ui, &name)),
                )));
            }
        },
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |items: &[Artist], pos| open_artist_detail(&state, &ui, &items[pos].name)
        },
    );
    ui.artists_scroller.set_child(Some(&lv));
}

/// Build and install the Folders virtualised list from a fetched folder list.
pub(super) fn render_folders(state: &SharedState, ui: &SharedUi, folders: Vec<Folder>) {
    let lv = list_view(
        folders,
        row_setup,
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |h: &RowHandle, f: &Folder| {
                h.set_art(None, &ui.art);
                h.set_texts(f.name(), &f.path, Some(&f.meta()));
                h.set_heart(None);
                let (state, ui, path) = (state.clone(), ui.clone(), f.path.clone());
                h.set_trailing(Some((
                    "media-playback-start-symbolic",
                    "Play folder",
                    Box::new(move || play_folder(&state, &ui, &path)),
                )));
            }
        },
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |items: &[Folder], pos| play_folder(&state, &ui, &items[pos].path)
        },
    );
    ui.folders_scroller.set_child(Some(&lv));
}

/// Install the Loved tab from a fetched loved-track list (empty state or list).
pub(super) fn render_loved(state: &SharedState, ui: &SharedUi, tracks: Vec<Track>) {
    if tracks.is_empty() {
        ui.loved_scroller.set_child(Some(&status_page(
            "emblem-favorite-symbolic",
            "No Loved Tracks Yet",
            "Tap the heart on any track to add it here.",
        )));
    } else {
        ui.loved_scroller
            .set_child(Some(&track_list_view(state, ui, tracks)));
    }
}

/// A virtualised track list shared by the Tracks and Loved browse tabs: each row
/// carries a heart toggle and an add-to-queue action; activating a row plays the
/// list from that position.
pub(super) fn track_list_view(state: &SharedState, ui: &SharedUi, tracks: Vec<Track>) -> gtk::ListView {
    list_view(
        tracks,
        row_setup,
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |h: &RowHandle, t: &Track| {
                h.set_art(t.art_hash.as_deref(), &ui.art);
                h.set_texts(&t.display_title(), &t.subtitle(), Some(&t.format_spec()));
                let (id, loved) = (t.id, t.loved);
                let (sh, uh) = (state.clone(), ui.clone());
                h.set_heart(Some((loved, Box::new(move |now| toggle_loved(&sh, &uh, id, now)))));
                let (s2, u2, tc) = (state.clone(), ui.clone(), t.clone());
                h.set_trailing(Some((
                    "list-add-symbolic",
                    "Add to queue",
                    Box::new(move || enqueue_track(&s2, &u2, tc.clone())),
                )));
            }
        },
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |items: &[Track], pos| play_list(&state, &ui, items.to_vec(), pos)
        },
    )
}
