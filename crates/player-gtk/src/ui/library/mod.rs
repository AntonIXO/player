//! The Library page: the Albums grid (+ A–Z fast index), the segmented browse
//! switcher, the sort menu, and the empty state. The virtualised browse-list
//! renderers live in [`browse`]; the album/artist detail pages in [`detail`].

mod browse;
mod detail;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::Orientation;
use player_library::{Album, Sort};

use crate::list::grid_view;
use crate::state::{SharedState, SharedUi};
use crate::ui::libworker::{spinner_after, submit_render, LibPayload};
use crate::widgets::{album_cell_bind, album_cell_setup, status_page, AlbumCellHandle};

pub(crate) use detail::{open_album_for, open_artist_detail};

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
    // The A–Z index has one button per first-letter; a Cyrillic+Latin library yields
    // ~60+ letters (~900 px tall), which would pin the Library page's min-height past
    // a phone screen — growing the window taller than the display and pushing the
    // bottom nav off the bottom. Wrap it in a vertical scroller (External policy = no
    // visible scrollbar, swipe) so it scrolls instead of pinning the page tall.
    let az_scroll = gtk::ScrolledWindow::new();
    az_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
    az_scroll.set_child(Some(&az));

    let albums_row = gtk::Box::new(Orientation::Horizontal, 0);
    albums_row.append(&albums_scroller);
    albums_row.append(&az_scroll);

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

/// Current window content width for sizing the width-derived art, **capped to the
/// monitor's logical width**. The cap is load-bearing: the album covers are
/// fixed-size, so they pin the Albums page's *minimum* width to `3 × cover`. If a
/// transient pre-maximize (or a non-maximized) allocation reported the content's
/// natural width (wider than the screen), the covers would size up, pinning the
/// Albums min-width past the screen — and because `AdwViewStack` is hhomogeneous,
/// that over-wide page drags *every* page wider than the window, overflowing it
/// (the whole UI shifts left / spills off the right). Capping to the monitor keeps
/// `3 × cover + reserve ≤ screen`, so the grid never pins the window wider than the
/// display and the window can always settle at the screen width.
pub(crate) fn window_width(ui: &SharedUi) -> i32 {
    let w = ui.window.width();
    let w = if w > 0 { w } else { ui.window.default_width() };
    if let Some(surface) = ui.window.surface() {
        if let Some(mon) = gtk::gdk::Display::default()
            .and_then(|d| d.monitor_at_surface(&surface))
        {
            let mw = mon.geometry().width();
            if mw > 0 {
                return w.min(mw);
            }
        }
    }
    w
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
        move || album_cell_setup(px),
        {
            let cache = ui.art.clone();
            move |h: &AlbumCellHandle, al: &Album| album_cell_bind(h, al, &cache)
        },
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |_albums: &[Album], pos| detail::open_album_detail(&state, &ui, pos)
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
                        browse::render_artists(state, ui, v);
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
                        browse::render_folders(state, ui, v);
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
                            .set_child(Some(&browse::track_list_view(state, ui, v)));
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
                        browse::render_loved(state, ui, v);
                    }
                },
            );
            spinner_after(ui, &ui.loved_scroller, seq);
        }
        _ => {}
    }
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
