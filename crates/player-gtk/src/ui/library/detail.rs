//! The album- and artist-detail navigation pages (pushed onto the Library nav
//! stack), plus the small shared chrome they have in common — the Play/Shuffle/
//! Add action row, the back button, and the margined page assembly.

use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::Orientation;
use player_library::{fmt_dur_ms, Album, Track};

use crate::playback::{
    enqueue_track, enqueue_tracks, play_album, play_artist, play_list, shuffle_vec, toggle_loved,
};
use crate::state::{SharedState, SharedUi};
use crate::ui::libworker::{submit_action, submit_render, LibPayload};
use crate::widgets::{art_widget, boxed_list, clamp, row_widget, wrap_scroller};

/// The Play · Shuffle · Add-to-queue pill row shared by both detail pages.
/// Returns the row and the three buttons so the caller wires their actions.
struct DetailActions {
    row: gtk::Box,
    play: gtk::Button,
    shuffle: gtk::Button,
    add: gtk::Button,
}

fn detail_actions(add_tooltip: &str) -> DetailActions {
    let row = gtk::Box::new(Orientation::Horizontal, 8);
    let play = gtk::Button::with_label("Play");
    play.add_css_class("suggested-action");
    play.add_css_class("pill");
    let shuffle = gtk::Button::from_icon_name("media-playlist-shuffle-symbolic");
    shuffle.set_tooltip_text(Some("Shuffle play"));
    shuffle.add_css_class("pill");
    let add = gtk::Button::from_icon_name("list-add-symbolic");
    add.set_tooltip_text(Some(add_tooltip));
    add.add_css_class("pill");
    row.append(&play);
    row.append(&shuffle);
    row.append(&add);
    DetailActions { row, play, shuffle, add }
}

/// The flat back button both detail pages carry. The shared header bar lives
/// outside the `NavigationView`, so a pushed page would otherwise have no
/// on-screen way back (only edge-swipe / Escape).
fn detail_back(ui: &SharedUi, tooltip: &str) -> gtk::Button {
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.add_css_class("flat");
    back.set_halign(gtk::Align::Start);
    back.set_tooltip_text(Some(tooltip));
    let ui = ui.clone();
    back.connect_clicked(move |_| {
        ui.nav.pop();
    });
    back
}

/// Assemble a detail page: a margined column of [back, header, actions, list],
/// wrapped in the scroller + width clamp, as a titled navigation page.
fn detail_page(
    title: &str,
    back: &gtk::Button,
    header: &gtk::Box,
    actions: &gtk::Box,
    list: &impl IsA<gtk::Widget>,
) -> adw::NavigationPage {
    let content = gtk::Box::new(Orientation::Vertical, 16);
    content.set_margin_top(12);
    content.set_margin_bottom(20);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(back);
    content.append(header);
    content.append(actions);
    content.append(list);
    adw::NavigationPage::new(&wrap_scroller(&clamp(&content)), title)
}

/// Open the detail page for the album at `index` in the cached album list. The
/// track query runs on the worker; the page is pushed when it returns (latest-
/// wins, so a second tap supersedes the first).
pub(super) fn open_album_detail(state: &SharedState, ui: &SharedUi, index: usize) {
    let Some(album) = state.borrow().albums.get(index).cloned() else {
        return;
    };
    let (a, aa) = (album.album.clone(), album.album_artist.clone());
    submit_render(
        ui,
        move |lib| LibPayload::Album {
            tracks: lib.album_tracks(&a, aa.as_deref()).unwrap_or_default(),
            album,
        },
        |state, ui, p| {
            if let LibPayload::Album { album, tracks } = p {
                let page = build_album_detail(state, ui, &album, tracks);
                ui.nav.push(&page);
            }
        },
    );
}

/// Build an album-detail navigation page: a hero header (art + titles), the
/// Play / Shuffle / Add-to-queue actions, and the track list.
pub(crate) fn build_album_detail(
    state: &SharedState,
    ui: &SharedUi,
    album: &Album,
    tracks: Vec<Track>,
) -> adw::NavigationPage {
    let tracks = Rc::new(tracks);

    let header = gtk::Box::new(Orientation::Horizontal, 16);
    let art = art_widget(&ui.art, album.art_hash.as_deref(), 132, true);
    header.append(&art);
    let titles = gtk::Box::new(Orientation::Vertical, 4);
    titles.set_valign(gtk::Align::Center);
    // hexpand + ellipsize so a long artist/meta can't pin the header (and thus this
    // pushed nav page) wider than the phone screen — the title wraps, the rest trims.
    titles.set_hexpand(true);
    let t = gtk::Label::new(Some(&album.album));
    t.add_css_class("title-2");
    t.set_xalign(0.0);
    t.set_wrap(true);
    let a = gtk::Label::new(Some(album.album_artist.as_deref().unwrap_or("Unknown Artist")));
    a.add_css_class("dim-label");
    a.set_xalign(0.0);
    a.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let m = gtk::Label::new(Some(&album.meta()));
    m.add_css_class("dim-label");
    m.add_css_class("caption");
    m.set_xalign(0.0);
    m.set_ellipsize(gtk::pango::EllipsizeMode::End);
    titles.append(&t);
    titles.append(&a);
    titles.append(&m);
    header.append(&titles);

    let DetailActions { row: actions, play: play_btn, shuffle: shuffle_btn, add: add_btn } =
        detail_actions("Add album to queue");
    {
        let (state, ui, all) = (state.clone(), ui.clone(), tracks.clone());
        play_btn.connect_clicked(move |_| play_list(&state, &ui, (*all).clone(), 0));
    }
    {
        let (state, ui, all) = (state.clone(), ui.clone(), tracks.clone());
        shuffle_btn.connect_clicked(move |_| {
            let mut v = (*all).clone();
            shuffle_vec(&mut v);
            play_list(&state, &ui, v, 0);
        });
    }
    {
        let (state, ui, all) = (state.clone(), ui.clone(), tracks.clone());
        add_btn.connect_clicked(move |_| enqueue_tracks(&state, &ui, (*all).clone()));
    }

    // track list
    let lb = boxed_list();
    for (i, tr) in tracks.iter().enumerate() {
        let (id, loved) = (tr.id, tr.loved);
        let (state, ui, all) = (state.clone(), ui.clone(), tracks.clone());
        let (s2, u2, tc) = (state.clone(), ui.clone(), tr.clone());
        let (sh, uh) = (state.clone(), ui.clone());
        let cache = ui.art.clone();
        let n = tr.track_no.map(|n| format!("{n}.  ")).unwrap_or_default();
        let row = row_widget(
            &cache,
            tr.art_hash.as_deref(),
            &format!("{n}{}", tr.display_title()),
            &tr.subtitle(),
            Some(&fmt_dur_ms(tr.duration_ms.unwrap_or(0))),
            Some((loved, Box::new(move |now| toggle_loved(&sh, &uh, id, now)))),
            Some(("list-add-symbolic", "Add to queue")),
            move || play_list(&state, &ui, (*all).clone(), i),
            move || enqueue_track(&s2, &u2, tc.clone()),
        );
        lb.append(&row);
    }

    let back = detail_back(ui, "Back to library");
    detail_page(&album.album, &back, &header, &actions, &lb)
}

/// Push the detail page for `artist` and reveal the Library tab (works from the
/// browse list, search results, or the now-playing nav buttons).
pub(crate) fn open_artist_detail(_state: &SharedState, ui: &SharedUi, artist: &str) {
    let name = artist.to_string();
    let name2 = name.clone();
    submit_render(
        ui,
        move |lib| LibPayload::ArtistAlbums(lib.artist_albums(&name).unwrap_or_default()),
        move |state, ui, p| {
            if let LibPayload::ArtistAlbums(albums) = p {
                let page = build_artist_detail(state, ui, &name2, albums);
                ui.nav.push(&page);
                ui.stack.set_visible_child_name("library");
            }
        },
    );
}

/// Push the album detail page for `(album, album_artist)`, looking the `Album` up
/// in the cached list and synthesising it from the tracks if it isn't cached.
pub(crate) fn open_album_for(
    state: &SharedState,
    ui: &SharedUi,
    album: &str,
    album_artist: Option<&str>,
) {
    let found = {
        let s = state.borrow();
        s.albums
            .iter()
            .find(|a| a.album == album && a.album_artist.as_deref() == album_artist)
            .cloned()
    };
    let (a, aa) = (album.to_string(), album_artist.map(|s| s.to_string()));
    submit_render(
        ui,
        move |lib| {
            let tracks = lib.album_tracks(&a, aa.as_deref()).unwrap_or_default();
            // Synthesise the Album from the tracks when it isn't in the cached
            // browse list (e.g. opened from search or an artist page).
            let album = found.unwrap_or_else(|| Album {
                album: a.clone(),
                album_artist: aa.clone(),
                year: tracks.iter().find_map(|t| t.year),
                track_count: tracks.len() as u32,
                total_ms: tracks.iter().filter_map(|t| t.duration_ms).sum(),
                art_hash: tracks.iter().find_map(|t| t.art_hash.clone()),
            });
            LibPayload::Album { album, tracks }
        },
        |state, ui, p| {
            if let LibPayload::Album { album, tracks } = p {
                if tracks.is_empty() {
                    ui.toast("No tracks for this album");
                    return;
                }
                let page = build_album_detail(state, ui, &album, tracks);
                ui.nav.push(&page);
                ui.stack.set_visible_child_name("library");
            }
        },
    );
}

/// Build an artist-detail navigation page: an avatar hero, Play / Shuffle / Add
/// actions over all the artist's tracks, and the list of their albums (each
/// drilling into [`build_album_detail`]). Mirrors [`build_album_detail`].
fn build_artist_detail(
    state: &SharedState,
    ui: &SharedUi,
    artist: &str,
    albums: Vec<Album>,
) -> adw::NavigationPage {
    let header = gtk::Box::new(Orientation::Horizontal, 16);
    // We never have an actual artist photo, so use a real AdwAvatar: it shows the
    // artist's initials in a generated-colour circle — purpose-built for this and
    // it looks intentional instead of a broken placeholder. Crucially it requests
    // exactly `size`×`size` and never hexpand-propagates, unlike the old hand-rolled
    // Box (whose inner Image had `hexpand:true`, which a GtkBox propagates → the
    // avatar stretched horizontally and shoved the titles off to the side).
    let avatar = adw::Avatar::new(132, Some(artist), true);
    avatar.set_halign(gtk::Align::Center);
    avatar.set_valign(gtk::Align::Center);
    header.append(&avatar);

    let titles = gtk::Box::new(Orientation::Vertical, 4);
    titles.set_valign(gtk::Align::Center);
    titles.set_hexpand(true);
    let t = gtk::Label::new(Some(artist));
    t.add_css_class("title-2");
    t.set_xalign(0.0);
    t.set_wrap(true);
    let track_count: u32 = albums.iter().map(|a| a.track_count).sum();
    let m = gtk::Label::new(Some(&format!(
        "{} album{} · {} track{}",
        albums.len(),
        if albums.len() == 1 { "" } else { "s" },
        track_count,
        if track_count == 1 { "" } else { "s" },
    )));
    m.add_css_class("dim-label");
    m.add_css_class("caption");
    m.set_xalign(0.0);
    m.set_ellipsize(gtk::pango::EllipsizeMode::End);
    titles.append(&t);
    titles.append(&m);
    header.append(&titles);

    let DetailActions { row: actions, play: play_btn, shuffle: shuffle_btn, add: add_btn } =
        detail_actions("Add artist to queue");
    {
        let (state, ui, name) = (state.clone(), ui.clone(), artist.to_string());
        play_btn.connect_clicked(move |_| play_artist(&state, &ui, &name));
    }
    {
        let (ui, name) = (ui.clone(), artist.to_string());
        shuffle_btn.connect_clicked(move |_| {
            let name = name.clone();
            submit_action(
                &ui,
                move |lib| LibPayload::Tracks(lib.artist_tracks(&name).unwrap_or_default()),
                |state, ui, p| {
                    if let LibPayload::Tracks(mut v) = p {
                        shuffle_vec(&mut v);
                        play_list(state, ui, v, 0);
                    }
                },
            );
        });
    }
    {
        let (ui, name) = (ui.clone(), artist.to_string());
        add_btn.connect_clicked(move |_| {
            let name = name.clone();
            submit_action(
                &ui,
                move |lib| LibPayload::Tracks(lib.artist_tracks(&name).unwrap_or_default()),
                |state, ui, p| {
                    if let LibPayload::Tracks(v) = p {
                        enqueue_tracks(state, ui, v);
                    }
                },
            );
        });
    }

    // album list
    let lb = boxed_list();
    for al in &albums {
        let (s1, u1, a1) = (state.clone(), ui.clone(), al.clone());
        let (s2, u2, a2) = (state.clone(), ui.clone(), al.clone());
        let row = row_widget(
            &ui.art,
            al.art_hash.as_deref(),
            &al.album,
            al.album_artist.as_deref().unwrap_or(artist),
            Some(&al.meta()),
            None,
            Some(("media-playback-start-symbolic", "Play album")),
            move || open_album_for(&s1, &u1, &a1.album, a1.album_artist.as_deref()),
            move || play_album(&s2, &u2, &a2.album, a2.album_artist.as_deref()),
        );
        lb.append(&row);
    }

    let back = detail_back(ui, "Back");
    detail_page(artist, &back, &header, &actions, &lb)
}
