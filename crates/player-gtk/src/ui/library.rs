//! The Library page: the Albums grid (+ A–Z fast index), the Artists / Folders /
//! Tracks virtualised list tabs, the sort menu, and the album/artist detail
//! pages pushed onto the navigation stack.

use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::Orientation;
use player_library::{fmt_dur_ms, Album, Artist, Folder, Sort, Track};

use crate::list::{grid_view, list_view};
use crate::playback::{
    enqueue_track, enqueue_tracks, play_album, play_artist, play_folder, play_list, shuffle_vec,
    toggle_loved,
};
use crate::state::{SharedState, SharedUi};
use crate::ui::libworker::{spinner_after, submit_action, submit_render, LibPayload};
use crate::widgets::{
    album_cell, art_widget, boxed_list, clamp, row_inner, row_widget, status_page, wrap_scroller,
};

/// Widgets the library page exposes to the rest of the app.
pub(crate) struct LibUi {
    pub(crate) nav: adw::NavigationView,
    pub(crate) albums_scroller: gtk::ScrolledWindow,
    pub(crate) az_box: gtk::Box,
    pub(crate) outer: gtk::Stack, // "empty" | "browse"
    pub(crate) browse_stack: gtk::Stack,
    pub(crate) artists_scroller: gtk::ScrolledWindow,
    pub(crate) folders_scroller: gtk::ScrolledWindow,
    pub(crate) tracks_scroller: gtk::ScrolledWindow,
    pub(crate) loved_scroller: gtk::ScrolledWindow,
    pub(crate) seg: Vec<(&'static str, gtk::ToggleButton)>,
    pub(crate) sort_btn: gtk::MenuButton,
    pub(crate) sort_label: gtk::Label,
    pub(crate) sort_opts: Vec<(Sort, gtk::Button)>,
}

/// A vertically-scrolling viewport for a virtualised browse list.
fn browse_scroller() -> gtk::ScrolledWindow {
    let s = gtk::ScrolledWindow::new();
    s.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    s.set_vexpand(true);
    s
}

/// The browse tabs, in display order.
const BROWSE_TABS: [(&str, &str); 5] = [
    ("loved", "Loved"),
    ("albums", "Albums"),
    ("artists", "Artists"),
    ("folders", "Folders"),
    ("tracks", "Tracks"),
];

pub(crate) fn build_library() -> LibUi {
    // --- Albums grid (a GridView, set by refresh_library) + A–Z fast index ---
    let albums_scroller = browse_scroller();
    albums_scroller.set_hexpand(true);

    let az = gtk::Box::new(Orientation::Vertical, 1);
    az.add_css_class("az-index");
    az.set_valign(gtk::Align::Center);
    az.set_margin_end(2);

    let albums_row = gtk::Box::new(Orientation::Horizontal, 0);
    albums_row.append(&albums_scroller);
    albums_row.append(&az);

    // --- Artists / Folders / Tracks list tabs (virtualised, set on demand) ---
    let artists_scroller = browse_scroller();
    let folders_scroller = browse_scroller();
    let tracks_scroller = browse_scroller();
    let loved_scroller = browse_scroller();

    let browse_stack = gtk::Stack::new();
    browse_stack.set_vexpand(true);
    browse_stack.add_named(&albums_row, Some("albums"));
    browse_stack.add_named(&artists_scroller, Some("artists"));
    browse_stack.add_named(&folders_scroller, Some("folders"));
    browse_stack.add_named(&tracks_scroller, Some("tracks"));
    browse_stack.add_named(&loved_scroller, Some("loved"));
    browse_stack.set_visible_child_name("albums");

    // --- Segmented header (linked toggles) ---
    let segbar = gtk::Box::new(Orientation::Horizontal, 0);
    segbar.add_css_class("linked");
    segbar.set_halign(gtk::Align::Center);
    let mut seg = Vec::new();
    for (name, label) in BROWSE_TABS {
        let b = gtk::ToggleButton::with_label(label);
        if name == "albums" {
            b.set_active(true);
        }
        segbar.append(&b);
        seg.push((name, b));
    }
    // --- Sort menu (Title / Artist / Year) — applies to Albums & Tracks tabs ---
    let sort_label = gtk::Label::new(Some("Title"));
    let sort_btn = gtk::MenuButton::new();
    sort_btn.add_css_class("flat");
    sort_btn.set_tooltip_text(Some("Sort"));
    {
        let sb_box = gtk::Box::new(Orientation::Horizontal, 6);
        let icon = gtk::Image::from_icon_name("view-sort-descending-symbolic");
        sb_box.append(&icon);
        sb_box.append(&sort_label);
        sort_btn.set_child(Some(&sb_box));
    }
    let mut sort_opts = Vec::new();
    {
        let pbox = gtk::Box::new(Orientation::Vertical, 0);
        pbox.set_margin_top(4);
        pbox.set_margin_bottom(4);
        for (s, label) in [
            (Sort::Title, "Title"),
            (Sort::Artist, "Artist"),
            (Sort::Year, "Year — newest"),
        ] {
            let ob = gtk::Button::with_label(label);
            ob.add_css_class("flat");
            ob.set_hexpand(true);
            if let Some(c) = ob.child().and_downcast::<gtk::Label>() {
                c.set_xalign(0.0);
            }
            pbox.append(&ob);
            sort_opts.push((s, ob));
        }
        let pop = gtk::Popover::new();
        pop.set_child(Some(&pbox));
        sort_btn.set_popover(Some(&pop));
    }

    let header = gtk::Box::new(Orientation::Horizontal, 8);
    header.set_margin_top(12);
    header.set_margin_start(16);
    header.set_margin_end(16);
    header.set_margin_bottom(8);
    // The five browse tabs are wider than a ~360 px phone screen. Make the
    // segmented control horizontally scrollable so it never pins a minimum width
    // wider than the screen: AdwViewStack sizes to its widest page, so a too-wide
    // Library page would otherwise force *every* page wide and overflow the whole
    // app on the Poco F1. The scroller hexpands, so the bar still centres when
    // there's room (desktop) and only scrolls (swipe) when there isn't (phone);
    // the sort menu stays pinned to the right. (Earlier a *centred, non-scrolling*
    // bar with hexpand spacers was used — that's exactly what pinned the 521 px
    // minimum.)
    let segscroll = gtk::ScrolledWindow::new();
    segscroll.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    segscroll.set_hexpand(true);
    segscroll.set_child(Some(&segbar));
    sort_btn.set_halign(gtk::Align::End);
    header.append(&segscroll);
    header.append(&sort_btn);

    let browse = gtk::Box::new(Orientation::Vertical, 0);
    browse.append(&header);
    browse.append(&browse_stack);

    // --- Empty hint — a standard Adwaita status page. ---
    let empty = status_page(
        "folder-music-symbolic",
        "Your Library Is Empty",
        "Use the menu → Scan Music Folder to index your music.",
    );

    let outer = gtk::Stack::new();
    outer.add_named(&empty, Some("empty"));
    outer.add_named(&browse, Some("browse"));
    outer.set_vexpand(true);

    // Root navigation page; album detail pages are pushed onto this.
    let root = adw::NavigationPage::new(&outer, "Library");
    let nav = adw::NavigationView::new();
    nav.add(&root);

    LibUi {
        nav,
        albums_scroller,
        az_box: az,
        outer,
        browse_stack,
        artists_scroller,
        folders_scroller,
        tracks_scroller,
        loved_scroller,
        seg,
        sort_btn,
        sort_label,
        sort_opts,
    }
}

/// Wire the Albums/Artists/Folders/Tracks segmented switcher (radio behaviour).
pub(crate) fn wire_segmented(
    state: &SharedState,
    ui: &SharedUi,
    seg: &[(&'static str, gtk::ToggleButton)],
) {
    for (name, btn) in seg {
        let (state, ui, name) = (state.clone(), ui.clone(), *name);
        let seg: Vec<(&'static str, gtk::ToggleButton)> = seg.to_vec();
        btn.connect_toggled(move |b| {
            if !b.is_active() {
                return;
            }
            for (on, ob) in &seg {
                if *on != name {
                    ob.set_active(false);
                }
            }
            show_browse_tab(&state, &ui, name);
        });
    }
}

/// Wire the browse sort menu: each option sets the active sort, updates the menu
/// label, and repopulates the current tab (Albums via [`refresh_library`], Tracks
/// via [`show_browse_tab`]; Artists/Folders keep their natural order).
pub(crate) fn wire_sort(
    state: &SharedState,
    ui: &SharedUi,
    sort_btn: &gtk::MenuButton,
    opts: &[(Sort, gtk::Button)],
) {
    for (s, btn) in opts {
        let (state, ui, s, sort_btn) = (state.clone(), ui.clone(), *s, sort_btn.clone());
        btn.connect_clicked(move |_| {
            ui.sort.set(s);
            ui.sort_label.set_text(sort_label_text(s));
            sort_btn.popdown();
            let active = ui
                .browse_stack
                .visible_child_name()
                .map(|s| s.to_string())
                .unwrap_or_default();
            match active.as_str() {
                "albums" => refresh_library(&state, &ui),
                "tracks" => show_browse_tab(&state, &ui, "tracks"),
                _ => {}
            }
        });
    }
}

fn sort_label_text(s: Sort) -> &'static str {
    match s {
        Sort::Title => "Title",
        Sort::Artist => "Artist",
        Sort::Year => "Year — newest",
        Sort::Album => "Album",
    }
}

/// The album-cover edge length (px) for a 3-up grid at window width `window_w`.
/// Reserves ~96 px for the A–Z index and the grid/cell padding around three cells
/// so the row fits a ~360 px phone, and quantises to 8 px so an interactive resize
/// rebuilds the grid only every few steps. Clamped so covers never get unusably
/// small on a phone or huge on a wide monitor.
pub(crate) fn album_cover_px(window_w: i32) -> i32 {
    let raw = ((window_w - 104).max(120) / 3).clamp(80, 240);
    (raw / 8) * 8
}

/// Current window content width (allocated, falling back to the default before the
/// first allocation).
pub(crate) fn window_width(ui: &SharedUi) -> i32 {
    let w = ui.window.width();
    if w > 0 {
        w
    } else {
        ui.window.default_width()
    }
}

/// (Re)build the albums `GridView` at the current cover size (`ui.cover_px`) and
/// refresh the A–Z index. Reads the cached album list from state, so a resize can
/// rebuild without re-querying. No-op when the library is empty.
pub(crate) fn rebuild_albums(state: &SharedState, ui: &SharedUi) {
    let albums = state.borrow().albums.clone();
    if albums.is_empty() {
        return;
    }
    let px = ui.cover_px.get();
    // Always 3 per row; the square covers are sized to the width (see album_cover_px).
    let grid = grid_view(
        albums.clone(),
        3,
        3,
        {
            let cache = ui.art.clone();
            move |al: &Album| album_cell(&cache, al, px).upcast()
        },
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |_albums: &[Album], pos| open_album_detail(&state, &ui, pos)
        },
    );
    while let Some(c) = ui.az_box.first_child() {
        ui.az_box.remove(&c);
    }
    ui.albums_scroller.set_child(Some(&grid));
    build_az_index(ui, &albums, &grid, ui.sort.get());
}

pub(crate) fn refresh_library(_state: &SharedState, ui: &SharedUi) {
    let sort = ui.sort.get();
    submit_render(
        ui,
        move |lib| LibPayload::Albums {
            albums: lib.albums(sort).unwrap_or_default(),
            n_tracks: lib.stats().map(|s| s.tracks).unwrap_or(0),
        },
        |state, ui, p| {
            if let LibPayload::Albums { albums, n_tracks } = p {
                render_albums(state, ui, albums, n_tracks);
            }
        },
    );
}

/// Apply a fetched album list: toggle the empty/browse state, cache the albums,
/// size the covers to the current width, (re)build the 3-up grid, and repopulate
/// any non-album tab currently showing.
fn render_albums(state: &SharedState, ui: &SharedUi, albums: Vec<Album>, n_tracks: u64) {
    while let Some(c) = ui.az_box.first_child() {
        ui.az_box.remove(&c);
    }

    if n_tracks == 0 {
        ui.albums_scroller.set_child(gtk::Widget::NONE);
        if let Some(s) = ui.library_empty.downcast_ref::<gtk::Stack>() {
            s.set_visible_child_name("empty");
        }
        state.borrow_mut().queue_albums(albums);
        return;
    }
    if let Some(s) = ui.library_empty.downcast_ref::<gtk::Stack>() {
        s.set_visible_child_name("browse");
    }

    // Cache the album list, size the covers to the current width, and build the
    // grid. `rebuild_albums` is also called on resize so the 3-up grid scales.
    state.borrow_mut().queue_albums(albums);
    ui.cover_px.set(album_cover_px(window_width(ui)));
    rebuild_albums(state, ui);

    // Repopulate whichever non-album tab is currently showing.
    let active = ui
        .browse_stack
        .visible_child_name()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "albums".into());
    if active != "albums" {
        show_browse_tab(state, ui, &active);
    }
}

/// Reveal a browse tab and fill it. Albums are filled by [`refresh_library`];
/// Artists/Folders/Tracks/Loved fetch their rows on the library worker (so the
/// tab switch never blocks) and render the virtualised `ListView` when the result
/// arrives — latest-wins, so spamming tabs only renders the last one.
pub(crate) fn show_browse_tab(_state: &SharedState, ui: &SharedUi, name: &str) {
    ui.browse_stack.set_visible_child_name(name);
    match name {
        "artists" => {
            let seq = submit_render(
                ui,
                |lib| LibPayload::Artists(lib.artists().unwrap_or_default()),
                |state, ui, p| {
                    if let LibPayload::Artists(v) = p {
                        render_artists(state, ui, v);
                    }
                },
            );
            spinner_after(ui, &ui.artists_scroller, seq);
        }
        "folders" => {
            let seq = submit_render(
                ui,
                |lib| LibPayload::Folders(lib.folders().unwrap_or_default()),
                |state, ui, p| {
                    if let LibPayload::Folders(v) = p {
                        render_folders(state, ui, v);
                    }
                },
            );
            spinner_after(ui, &ui.folders_scroller, seq);
        }
        "tracks" => {
            let sort = ui.sort.get();
            let seq = submit_render(
                ui,
                move |lib| LibPayload::Tracks(lib.tracks(sort).unwrap_or_default()),
                |state, ui, p| {
                    if let LibPayload::Tracks(v) = p {
                        ui.tracks_scroller
                            .set_child(Some(&track_list_view(state, ui, v)));
                    }
                },
            );
            spinner_after(ui, &ui.tracks_scroller, seq);
        }
        "loved" => {
            let seq = submit_render(
                ui,
                |lib| LibPayload::Tracks(lib.loved_tracks().unwrap_or_default()),
                |state, ui, p| {
                    if let LibPayload::Tracks(v) = p {
                        render_loved(state, ui, v);
                    }
                },
            );
            spinner_after(ui, &ui.loved_scroller, seq);
        }
        _ => {}
    }
}

/// Build and install the Artists virtualised list from a fetched artist list.
fn render_artists(state: &SharedState, ui: &SharedUi, artists: Vec<Artist>) {
    let lv = list_view(
        artists,
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |a: &Artist| {
                let meta = format!(
                    "{} album{} · {} track{}",
                    a.album_count,
                    if a.album_count == 1 { "" } else { "s" },
                    a.track_count,
                    if a.track_count == 1 { "" } else { "s" },
                );
                let cache = ui.art.clone();
                let (state, ui, name) = (state.clone(), ui.clone(), a.name.clone());
                row_inner(
                    &cache,
                    None,
                    &name.clone(),
                    &meta,
                    None,
                    None,
                    Some(("media-playback-start-symbolic", "Play all")),
                    move || play_artist(&state, &ui, &name),
                )
                .upcast()
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
fn render_folders(state: &SharedState, ui: &SharedUi, folders: Vec<Folder>) {
    let lv = list_view(
        folders,
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |f: &Folder| {
                let cache = ui.art.clone();
                let (state, ui, path) = (state.clone(), ui.clone(), f.path.clone());
                row_inner(
                    &cache,
                    None,
                    f.name(),
                    &f.path,
                    Some(&f.meta()),
                    None,
                    Some(("media-playback-start-symbolic", "Play folder")),
                    move || play_folder(&state, &ui, &path),
                )
                .upcast()
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
fn render_loved(state: &SharedState, ui: &SharedUi, tracks: Vec<Track>) {
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
fn track_list_view(state: &SharedState, ui: &SharedUi, tracks: Vec<Track>) -> gtk::ListView {
    list_view(
        tracks,
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |t: &Track| {
                let (id, loved) = (t.id, t.loved);
                let (s2, u2, tc) = (state.clone(), ui.clone(), t.clone());
                let (sh, uh) = (state.clone(), ui.clone());
                row_inner(
                    &ui.art,
                    t.art_hash.as_deref(),
                    &t.display_title(),
                    &t.subtitle(),
                    Some(&t.format_spec()),
                    Some((loved, Box::new(move |now| toggle_loved(&sh, &uh, id, now)))),
                    Some(("list-add-symbolic", "Add to queue")),
                    move || enqueue_track(&s2, &u2, tc.clone()),
                )
                .upcast()
            }
        },
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |items: &[Track], pos| play_list(&state, &ui, items.to_vec(), pos)
        },
    )
}

/// The A–Z fast index only makes sense when the grid is in alphabetical order of
/// a text field. It keys on whatever the active sort orders by (album title, or
/// album artist) so consecutive entries collapse to one letter; for Year (numeric,
/// ungrouped) it emits nothing — keying on album title there would yield ~one
/// button per album and balloon the column past the screen.
fn build_az_index(ui: &SharedUi, albums: &[Album], grid: &gtk::GridView, sort: Sort) {
    let key = |al: &Album| -> Option<char> {
        let s = match sort {
            Sort::Year => return None,
            Sort::Artist => al.album_artist.as_deref().unwrap_or(""),
            _ => al.album.as_str(),
        };
        s.chars().next().map(|c| c.to_ascii_uppercase())
    };
    let mut last = '\0';
    for (i, al) in albums.iter().enumerate() {
        let Some(c) = key(al) else { continue };
        if c == last {
            continue;
        }
        last = c;
        let b = gtk::Button::with_label(&c.to_string());
        b.add_css_class("flat");
        let (grid, idx) = (grid.clone(), i as u32);
        b.connect_clicked(move |_| {
            grid.scroll_to(idx, gtk::ListScrollFlags::NONE, None);
        });
        ui.az_box.append(&b);
    }
}

/// Open the detail page for the album at `index` in the cached album list. The
/// track query runs on the worker; the page is pushed when it returns (latest-
/// wins, so a second tap supersedes the first).
fn open_album_detail(state: &SharedState, ui: &SharedUi, index: usize) {
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
    let t = gtk::Label::new(Some(&album.album));
    t.add_css_class("title-2");
    t.set_xalign(0.0);
    t.set_wrap(true);
    let a = gtk::Label::new(Some(album.album_artist.as_deref().unwrap_or("Unknown Artist")));
    a.add_css_class("dim-label");
    a.set_xalign(0.0);
    let m = gtk::Label::new(Some(&album.meta()));
    m.add_css_class("dim-label");
    m.add_css_class("caption");
    m.set_xalign(0.0);
    titles.append(&t);
    titles.append(&a);
    titles.append(&m);
    header.append(&titles);

    // actions: Play · Shuffle · Add to queue
    let actions = gtk::Box::new(Orientation::Horizontal, 8);
    let play_btn = gtk::Button::with_label("Play");
    play_btn.add_css_class("suggested-action");
    play_btn.add_css_class("pill");
    let shuffle_btn = gtk::Button::from_icon_name("media-playlist-shuffle-symbolic");
    shuffle_btn.set_tooltip_text(Some("Shuffle play"));
    shuffle_btn.add_css_class("pill");
    let add_btn = gtk::Button::from_icon_name("list-add-symbolic");
    add_btn.set_tooltip_text(Some("Add album to queue"));
    add_btn.add_css_class("pill");
    actions.append(&play_btn);
    actions.append(&shuffle_btn);
    actions.append(&add_btn);

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

    // Back affordance: the shared header bar lives outside the NavigationView,
    // so a pushed detail page would otherwise have no on-screen way back (only
    // edge-swipe / Escape). An explicit flat back button keeps it usable on the
    // desktop too.
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.add_css_class("flat");
    back.set_halign(gtk::Align::Start);
    back.set_tooltip_text(Some("Back to library"));
    {
        let ui = ui.clone();
        back.connect_clicked(move |_| {
            ui.nav.pop();
        });
    }

    let content = gtk::Box::new(Orientation::Vertical, 16);
    content.set_margin_top(12);
    content.set_margin_bottom(20);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(&back);
    content.append(&header);
    content.append(&actions);
    content.append(&lb);

    adw::NavigationPage::new(&wrap_scroller(&clamp(&content)), &album.album)
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
    titles.append(&t);
    titles.append(&m);
    header.append(&titles);

    // actions: Play · Shuffle · Add to queue (across all the artist's tracks)
    let actions = gtk::Box::new(Orientation::Horizontal, 8);
    let play_btn = gtk::Button::with_label("Play");
    play_btn.add_css_class("suggested-action");
    play_btn.add_css_class("pill");
    let shuffle_btn = gtk::Button::from_icon_name("media-playlist-shuffle-symbolic");
    shuffle_btn.set_tooltip_text(Some("Shuffle play"));
    shuffle_btn.add_css_class("pill");
    let add_btn = gtk::Button::from_icon_name("list-add-symbolic");
    add_btn.set_tooltip_text(Some("Add artist to queue"));
    add_btn.add_css_class("pill");
    actions.append(&play_btn);
    actions.append(&shuffle_btn);
    actions.append(&add_btn);
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

    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.add_css_class("flat");
    back.set_halign(gtk::Align::Start);
    back.set_tooltip_text(Some("Back"));
    {
        let ui = ui.clone();
        back.connect_clicked(move |_| {
            ui.nav.pop();
        });
    }

    let content = gtk::Box::new(Orientation::Vertical, 16);
    content.set_margin_top(12);
    content.set_margin_bottom(20);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(&back);
    content.append(&header);
    content.append(&actions);
    content.append(&lb);

    adw::NavigationPage::new(&wrap_scroller(&clamp(&content)), artist)
}
