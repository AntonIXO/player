//! Bit-Perfect Player — the libadwaita DAP shell.
//!
//! Recreates the Adwaita design archive: a bottom view switcher over
//! **Library · Playing · Search · Lists** (no equalizer — a no-DSP bit-perfect
//! player), a mini-player bar, toasts, and the signature **bit-perfect format
//! chip**. Browsing/search are backed by `player-library`; playback by the
//! `player-core` engine.
//!
//! Transport: the GTK app drives the queue track-by-track (advance on `Ended`),
//! but playback now uses the engine's real Pause/Resume/Seek with per-track
//! position — pause holds the device, the seek bar scrubs. Output device,
//! library root, theme, and the last session are persisted in the library's
//! `meta` table; the device defaults to a persisted choice, then `$PLAYER_DEVICE`,
//! then an auto-picked USB DAC. Gapless-through-UI + an engine-owned queue
//! remain deferred (FURTHER.md 3.7).

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::{gio, glib, Orientation};
use player_core::{Event, Player, StreamSpec};
use player_library::{
    fmt_dur_ms, fmt_khz, Album, Artist, Filter, Folder, Library, SearchIndex, SearchResults, Sort,
    Track,
};

mod art;
mod list;
mod widgets;
use art::ArtCache;
use list::{grid_view, list_view};
use widgets::*;

/// Mutable app state (single-threaded: GTK main loop).
struct State {
    library: Library,
    player: Player,
    db_path: PathBuf,
    art_dir: PathBuf,
    music_dir: Option<PathBuf>,
    queue: Vec<Track>,
    albums: Vec<Album>,
    current: Option<usize>,
    playing: bool,
    /// A track is loaded in the engine but held (pause, not stop) — resume
    /// continues it rather than restarting.
    paused: bool,
    repeat: bool,
    shuffle: bool,
    device: String,
    /// Sender the engine's `emit` closure feeds; cloned when re-spawning the
    /// player (device change) so the existing event pump keeps working.
    ev_tx: async_channel::Sender<Event>,
    /// Latest elapsed position (ms) of the current track, for session save.
    last_pos_ms: u64,
    /// A position to seek to once the restored track first starts (resume).
    resume_to: Option<Duration>,
    /// Kept alive while live folder-watching is enabled.
    watcher: Option<player_library::LibraryWatcher>,
}

/// Long-lived widget handles the closures update.
struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: adw::ViewStack,
    title: adw::WindowTitle,
    // mini player
    mini: gtk::Revealer,
    mp_art: gtk::Box,
    mp_title: gtk::Label,
    mp_artist: gtk::Label,
    mp_play: gtk::Button,
    mp_progress: gtk::ProgressBar,
    // now playing
    np_art: gtk::Box,
    np_title: gtk::Label,
    np_subtitle: gtk::Label,
    np_elapsed: gtk::Label,
    np_total: gtk::Label,
    np_seek: gtk::Scale,
    np_play: gtk::Button,
    np_goto_artist: gtk::Button,
    np_goto_album: gtk::Button,
    np_format: gtk::Box,
    np_stack: gtk::Stack,
    /// True while the user is dragging the seek slider — suppresses programmatic
    /// position updates so the handle does not fight the drag.
    seeking: Cell<bool>,
    /// Monotonic seek-debounce generation: each slider move bumps it and arms a
    /// timeout; only the latest generation actually commits the engine seek.
    seek_gen: Cell<u64>,
    // library — each browse tab is a ScrolledWindow whose child is a freshly
    // built (virtualised) ListView/GridView, so only visible rows are realised.
    nav: adw::NavigationView,
    albums_scroller: gtk::ScrolledWindow,
    az_box: gtk::Box,
    library_empty: gtk::Widget, // outer Stack: "empty" | "browse"
    browse_stack: gtk::Stack,   // "albums" | "artists" | "folders" | "tracks"
    artists_scroller: gtk::ScrolledWindow,
    folders_scroller: gtk::ScrolledWindow,
    tracks_scroller: gtk::ScrolledWindow,
    /// Current browse sort (Albums/Tracks tabs); the menu button's label mirrors it.
    sort: Cell<Sort>,
    sort_label: gtk::Label,
    // search
    search_entry: gtk::SearchEntry,
    search_results: gtk::Box,
    filter: RefCell<Filter>,
    filter_buttons: Vec<(Filter, gtk::ToggleButton)>,
    /// Sender into the background search worker (own connection + fuzzy index).
    search_tx: async_channel::Sender<SearchMsg>,
    /// Monotonic search generation: each keystroke/filter change bumps it; a
    /// result is rendered only if its seq still matches (latest-wins).
    search_seq: Cell<u64>,
    // queue
    queue_box: gtk::Box,
    queue_title: gtk::Label,
    /// Async, cached cover-art textures (decoded off the main thread).
    art: ArtCache,
}

type SharedState = Rc<RefCell<State>>;
type SharedUi = Rc<Ui>;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.player.BitPerfect")
        .build();
    app.connect_startup(|_| load_css());
    app.connect_activate(build_ui);
    app.run()
}

/// Apply a theme: explicit "light"/"dark" force a scheme, "system" follows the
/// desktop, and an unset value (`None`, first run) prefers dark — the DAP is a
/// dark-by-design player, but the user can still override it in Settings.
fn apply_theme(theme: Option<&str>) {
    let scheme = match theme {
        Some("light") => adw::ColorScheme::ForceLight,
        Some("dark") => adw::ColorScheme::ForceDark,
        Some("system") => adw::ColorScheme::Default,
        None => adw::ColorScheme::PreferDark,
        _ => adw::ColorScheme::Default,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn build_ui(app: &adw::Application) {
    let (db_path, art_dir) = Library::default_paths();
    let library = Library::open(&db_path, &art_dir).unwrap_or_else(|e| {
        eprintln!("library open failed: {e}");
        std::process::exit(1);
    });

    // Output device: a persisted choice wins, then $PLAYER_DEVICE, then an
    // auto-picked USB DAC, then a sane default.
    let device = library
        .get_meta("device")
        .ok()
        .flatten()
        .or_else(|| std::env::var("PLAYER_DEVICE").ok())
        .or_else(|| player_core::auto_pick().map(|d| d.id))
        .unwrap_or_else(|| "hw:0,0".into());

    // Library root: a persisted choice, else the XDG Music dir.
    let music_dir = library
        .get_meta("music_dir")
        .ok()
        .flatten()
        .map(PathBuf::from)
        .or_else(|| glib::user_special_dir(glib::UserDirectory::Music));

    // Theme: restore the persisted color scheme.
    apply_theme(library.get_meta("theme").ok().flatten().as_deref());

    // Background search worker: owns its own read connection + the in-memory
    // fuzzy index, so per-keystroke search never blocks the main thread. It is
    // told to Reindex after a scan.
    let (search_tx, search_rx) = spawn_search_worker(db_path.clone(), art_dir.clone());

    // engine events → main loop
    let (ev_tx, ev_rx) = async_channel::unbounded::<Event>();
    let player = {
        let ev_tx = ev_tx.clone();
        Player::spawn(device.clone(), move |ev| {
            let _ = ev_tx.send_blocking(ev);
        })
    };

    let state: SharedState = Rc::new(RefCell::new(State {
        library,
        player,
        db_path,
        art_dir,
        music_dir,
        queue: Vec::new(),
        albums: Vec::new(),
        current: None,
        playing: false,
        paused: false,
        repeat: false,
        shuffle: false,
        device,
        ev_tx,
        last_pos_ms: 0,
        resume_to: None,
        watcher: None,
    }));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Bit-Perfect Player")
        .default_width(420)
        .default_height(780)
        .build();

    let title = adw::WindowTitle::new("Bit-Perfect Player", "■ idle");

    // --- header ---
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    let open_btn = gtk::Button::from_icon_name("document-open-symbolic");
    open_btn.set_tooltip_text(Some("Open a file"));
    header.pack_start(&open_btn);
    let menu_btn = gtk::MenuButton::new();
    menu_btn.set_icon_name("open-menu-symbolic");
    header.pack_end(&menu_btn);
    // Search toggle: jumps to the Search page and focuses the entry (sits just
    // left of the menu button, per the figma).
    let search_btn = gtk::Button::from_icon_name("system-search-symbolic");
    search_btn.set_tooltip_text(Some("Search"));
    header.pack_end(&search_btn);

    // --- the four pages ---
    let (np_page, np) = build_now_playing();
    let lib = build_library();
    let (search_page, search_entry, search_results, filter_buttons) = build_search();
    let queue_box = gtk::Box::new(Orientation::Vertical, 6);
    let pl_load = gtk::Button::from_icon_name("document-open-symbolic");
    pl_load.add_css_class("flat");
    pl_load.set_tooltip_text(Some("Load playlist (.m3u)"));
    let pl_save = gtk::Button::from_icon_name("document-save-symbolic");
    pl_save.add_css_class("flat");
    pl_save.set_tooltip_text(Some("Save queue as playlist (.m3u)"));
    let pl_clear = gtk::Button::from_icon_name("edit-clear-all-symbolic");
    pl_clear.add_css_class("flat");
    pl_clear.set_tooltip_text(Some("Clear the queue"));
    let queue_title = section_label("Up Next");
    queue_title.set_hexpand(true);
    let queue_page = wrap_scroller(&{
        let b = gtk::Box::new(Orientation::Vertical, 0);
        b.set_margin_top(14);
        b.set_margin_bottom(20);
        b.set_margin_start(14);
        b.set_margin_end(14);
        let head = gtk::Box::new(Orientation::Horizontal, 6);
        head.append(&queue_title);
        head.append(&pl_load);
        head.append(&pl_save);
        head.append(&pl_clear);
        b.append(&head);
        b.append(&queue_box);
        b
    });

    let stack = adw::ViewStack::new();
    add_page(&stack, &lib.nav, "library", "Library", "view-grid-symbolic");
    add_page(&stack, &np_page, "playing", "Playing", "media-playback-start-symbolic");
    add_page(&stack, &search_page, "search", "Search", "system-search-symbolic");
    add_page(&stack, &queue_page, "lists", "Queue", "view-list-symbolic");
    stack.set_visible_child_name("library");

    // --- mini player ---
    let (mini, mp_art, mp_title, mp_artist, mp_play, mp_progress) = build_mini();

    let switcher = adw::ViewSwitcherBar::new();
    switcher.set_stack(Some(&stack));
    switcher.set_reveal(true);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    toolbar.add_bottom_bar(&mini);
    toolbar.add_bottom_bar(&switcher);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(&toasts));

    let ui: SharedUi = Rc::new(Ui {
        window: window.clone(),
        toasts,
        stack: stack.clone(),
        title,
        mini,
        mp_art,
        mp_title,
        mp_artist,
        mp_play: mp_play.clone(),
        mp_progress,
        np_art: np.art,
        np_title: np.title,
        np_subtitle: np.subtitle,
        np_elapsed: np.elapsed,
        np_total: np.total,
        np_seek: np.seek,
        np_play: np.play.clone(),
        np_goto_artist: np.goto_artist.clone(),
        np_goto_album: np.goto_album.clone(),
        np_format: np.format,
        np_stack: np.stack.clone(),
        seeking: Cell::new(false),
        seek_gen: Cell::new(0),
        nav: lib.nav.clone(),
        albums_scroller: lib.albums_scroller.clone(),
        az_box: lib.az_box.clone(),
        library_empty: lib.outer.clone().upcast(),
        browse_stack: lib.browse_stack.clone(),
        artists_scroller: lib.artists_scroller.clone(),
        folders_scroller: lib.folders_scroller.clone(),
        tracks_scroller: lib.tracks_scroller.clone(),
        sort: Cell::new(Sort::Title),
        sort_label: lib.sort_label.clone(),
        search_entry: search_entry.clone(),
        search_results,
        filter: RefCell::new(Filter::All),
        filter_buttons,
        search_tx: search_tx.clone(),
        search_seq: Cell::new(0),
        queue_box,
        queue_title,
        art: ArtCache::new(state.borrow().art_dir.clone()),
    });

    wire(&state, &ui, &open_btn, &menu_btn, &mp_play, &np.play, &np.shuffle, &np.repeat, &np.prev, &np.next, &np.rewind, &np.fwd);
    wire_segmented(&state, &ui, &lib.seg);
    wire_sort(&state, &ui, &lib.sort_btn, &lib.sort_opts);

    // header search toggle → Search page + focus the entry
    {
        let ui = ui.clone();
        search_btn.connect_clicked(move |_| {
            ui.stack.set_visible_child_name("search");
            ui.search_entry.grab_focus();
        });
    }

    // now-playing → current track's artist / album detail page
    {
        let (state, ui) = (state.clone(), ui.clone());
        np.goto_artist.connect_clicked(move |_| {
            let Some(t) = current_track(&state) else { return };
            match t.album_artist.clone().or_else(|| t.artist.clone()) {
                Some(a) if !a.is_empty() => open_artist_detail(&state, &ui, &a),
                _ => ui.toast("No artist for this track"),
            }
        });
    }
    {
        let (state, ui) = (state.clone(), ui.clone());
        np.goto_album.connect_clicked(move |_| {
            let Some(t) = current_track(&state) else { return };
            match t.album.clone() {
                Some(al) if !al.is_empty() => {
                    open_album_for(&state, &ui, &al, t.album_artist.as_deref())
                }
                _ => ui.toast("No album for this track"),
            }
        });
    }

    // clear the queue
    {
        let (state, ui) = (state.clone(), ui.clone());
        pl_clear.connect_clicked(move |_| clear_queue(&state, &ui));
    }

    // playlist save / load (Lists page)
    {
        let (state, ui) = (state.clone(), ui.clone());
        let win = window.clone();
        pl_save.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Save playlist")
                .initial_name("playlist.m3u")
                .build();
            let (state, ui) = (state.clone(), ui.clone());
            dialog.save(Some(&win), gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        save_playlist(&state, &ui, path);
                    }
                }
            });
        });
    }
    {
        let (state, ui) = (state.clone(), ui.clone());
        let win = window.clone();
        pl_load.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Load playlist").build();
            let (state, ui) = (state.clone(), ui.clone());
            dialog.open(Some(&win), gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        load_playlist(&state, &ui, path);
                    }
                }
            });
        });
    }

    // persist the session when the window closes
    {
        let state = state.clone();
        window.connect_close_request(move |_| {
            save_session(&state);
            glib::Propagation::Proceed
        });
    }

    // engine event pump
    {
        let (state, ui) = (state.clone(), ui.clone());
        glib::spawn_future_local(async move {
            while let Ok(ev) = ev_rx.recv().await {
                match ev {
                    Event::Started { spec, .. } => on_started(&state, &ui, spec),
                    Event::Position(frames) => on_position(&state, &ui, frames),
                    Event::Ended => on_ended(&state, &ui),
                    Event::Error(e) => ui.toast(&format!("⚠ {e}")),
                }
            }
        });
    }

    // search results pump: render only the latest-requested generation.
    {
        let (state, ui) = (state.clone(), ui.clone());
        glib::spawn_future_local(async move {
            while let Ok(hits) = search_rx.recv().await {
                if hits.seq == ui.search_seq.get() {
                    render_search_results(&state, &ui, hits.results);
                }
            }
        });
    }

    refresh_library(&state, &ui);
    update_now_playing_empty(&ui, true);
    restore_session(&state, &ui);
    refresh_queue(&state, &ui);
    window.present();
}

// ---------------------------------------------------------------------------
// Page construction
// ---------------------------------------------------------------------------

struct Np {
    art: gtk::Box,
    title: gtk::Label,
    subtitle: gtk::Label,
    elapsed: gtk::Label,
    total: gtk::Label,
    seek: gtk::Scale,
    play: gtk::Button,
    prev: gtk::Button,
    next: gtk::Button,
    rewind: gtk::Button,
    fwd: gtk::Button,
    shuffle: gtk::Button,
    repeat: gtk::Button,
    goto_artist: gtk::Button,
    goto_album: gtk::Button,
    format: gtk::Box,
    stack: gtk::Stack,
}

fn build_now_playing() -> (gtk::Widget, Np) {
    let art = gtk::Box::new(Orientation::Vertical, 0);
    art.set_halign(gtk::Align::Center);
    art.set_size_request(ART_HERO, ART_HERO);

    let title = gtk::Label::new(Some("No Track Playing"));
    title.add_css_class("title-1");
    title.set_wrap(true);
    title.set_justify(gtk::Justification::Center);
    let subtitle = gtk::Label::new(None);
    subtitle.add_css_class("dim-label");
    subtitle.set_wrap(true);
    subtitle.set_justify(gtk::Justification::Center);

    // seek + times
    let seek = gtk::Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 0.001);
    seek.set_draw_value(false);
    seek.set_hexpand(true);
    seek.set_tooltip_text(Some("Drag to seek"));
    let elapsed = gtk::Label::new(Some("0:00"));
    elapsed.add_css_class("mono");
    elapsed.add_css_class("dim-label");
    let total = gtk::Label::new(Some("0:00"));
    total.add_css_class("mono");
    total.add_css_class("dim-label");
    let times = gtk::Box::new(Orientation::Horizontal, 0);
    times.append(&elapsed);
    let spacer = gtk::Box::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    times.append(&spacer);
    times.append(&total);

    // secondary toggle row — shuffle / repeat as pills, above the seek bar
    // (Poweramp-Adwaita design: the transport row is reserved for playback).
    let shuffle = circle("media-playlist-shuffle-symbolic", 40, "Shuffle");
    shuffle.add_css_class("pill-toggle");
    let repeat = circle("media-playlist-repeat-symbolic", 40, "Repeat");
    repeat.add_css_class("pill-toggle");
    // Jump to the current track's artist / album detail page.
    let goto_artist = circle("avatar-default-symbolic", 40, "Go to artist");
    goto_artist.add_css_class("pill-toggle");
    goto_artist.set_sensitive(false);
    let goto_album = circle("media-optical-symbolic", 40, "Go to album");
    goto_album.add_css_class("pill-toggle");
    goto_album.set_sensitive(false);
    let toggles = gtk::Box::new(Orientation::Horizontal, 9);
    toggles.set_halign(gtk::Align::Center);
    toggles.append(&shuffle);
    toggles.append(&repeat);
    toggles.append(&goto_artist);
    toggles.append(&goto_album);

    // transport: prev · −10s · play · +10s · next
    let prev = circle("media-skip-backward-symbolic", 48, "Previous");
    let rewind = circle("media-seek-backward-symbolic", 48, "Back 10 seconds");
    let play = circle("media-playback-start-symbolic", 72, "Play");
    play.add_css_class("play-hero");
    play.remove_css_class("flat");
    let fwd = circle("media-seek-forward-symbolic", 48, "Forward 10 seconds");
    let next = circle("media-skip-forward-symbolic", 48, "Next");
    let transport = gtk::Box::new(Orientation::Horizontal, 8);
    transport.set_halign(gtk::Align::Center);
    for b in [&prev, &rewind, &play, &fwd, &next] {
        transport.append(b);
    }

    let format = gtk::Box::new(Orientation::Horizontal, 8);
    format.set_halign(gtk::Align::Center);

    let note_box = gtk::Box::new(Orientation::Horizontal, 7);
    note_box.set_halign(gtk::Align::Center);
    let headphones = gtk::Image::from_icon_name("audio-headphones-symbolic");
    headphones.add_css_class("dim-label");
    let note = gtk::Label::new(Some("Volume is the DAC's hardware knob — no software gain, by design."));
    note.add_css_class("dim-label");
    note.add_css_class("caption");
    note.set_wrap(true);
    note.set_justify(gtk::Justification::Center);
    note_box.append(&headphones);
    note_box.append(&note);

    let content = gtk::Box::new(Orientation::Vertical, 16);
    content.set_valign(gtk::Align::Center);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(28);
    content.set_margin_end(28);
    content.append(&art);
    let titles = gtk::Box::new(Orientation::Vertical, 3);
    titles.append(&title);
    titles.append(&subtitle);
    content.append(&titles);
    content.append(&toggles);
    let seekbox = gtk::Box::new(Orientation::Vertical, 4);
    seekbox.append(&seek);
    seekbox.append(&times);
    content.append(&seekbox);
    content.append(&transport);
    content.append(&format);
    content.append(&note_box);

    // empty state
    let empty = gtk::Box::new(Orientation::Vertical, 14);
    empty.set_valign(gtk::Align::Center);
    empty.set_vexpand(true);
    let empty_icon = gtk::Image::from_icon_name("folder-music-symbolic");
    empty_icon.set_pixel_size(72);
    empty_icon.add_css_class("dim-label");
    let empty_title = gtk::Label::new(Some("No Track Playing"));
    empty_title.add_css_class("title-1");
    let empty_sub = gtk::Label::new(Some("Open a file or pick something from your library."));
    empty_sub.add_css_class("dim-label");
    empty.append(&empty_icon);
    empty.append(&empty_title);
    empty.append(&empty_sub);

    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    let content_scroller = wrap_scroller(&content);
    stack.add_named(&content_scroller, Some("content"));
    stack.set_visible_child_name("empty");

    (
        stack.clone().upcast(),
        Np {
            art,
            title,
            subtitle,
            elapsed,
            total,
            seek,
            play,
            prev,
            next,
            rewind,
            fwd,
            shuffle,
            repeat,
            goto_artist,
            goto_album,
            format,
            stack: stack.clone(),
        },
    )
}

/// Widgets the library page exposes to the rest of the app.
struct LibUi {
    nav: adw::NavigationView,
    albums_scroller: gtk::ScrolledWindow,
    az_box: gtk::Box,
    outer: gtk::Stack, // "empty" | "browse"
    browse_stack: gtk::Stack,
    artists_scroller: gtk::ScrolledWindow,
    folders_scroller: gtk::ScrolledWindow,
    tracks_scroller: gtk::ScrolledWindow,
    seg: Vec<(&'static str, gtk::ToggleButton)>,
    sort_btn: gtk::MenuButton,
    sort_label: gtk::Label,
    sort_opts: Vec<(Sort, gtk::Button)>,
}

/// A vertically-scrolling viewport for a virtualised browse list.
fn browse_scroller() -> gtk::ScrolledWindow {
    let s = gtk::ScrolledWindow::new();
    s.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    s.set_vexpand(true);
    s
}

/// The four browse tabs, in display order.
const BROWSE_TABS: [(&str, &str); 4] = [
    ("albums", "Albums"),
    ("artists", "Artists"),
    ("folders", "Folders"),
    ("tracks", "Tracks"),
];

fn build_library() -> LibUi {
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

    let browse_stack = gtk::Stack::new();
    browse_stack.set_vexpand(true);
    browse_stack.add_named(&albums_row, Some("albums"));
    browse_stack.add_named(&artists_scroller, Some("artists"));
    browse_stack.add_named(&folders_scroller, Some("folders"));
    browse_stack.add_named(&tracks_scroller, Some("tracks"));
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

    let header = gtk::Box::new(Orientation::Horizontal, 12);
    header.set_margin_top(12);
    header.set_margin_start(16);
    header.set_margin_end(16);
    header.set_margin_bottom(8);
    // Keep the segmented control centred, with the sort menu pinned to the right.
    let lead = gtk::Box::new(Orientation::Horizontal, 0);
    lead.set_hexpand(true);
    let trail = gtk::Box::new(Orientation::Horizontal, 0);
    trail.set_hexpand(true);
    trail.append(&sort_btn);
    sort_btn.set_halign(gtk::Align::End);
    header.append(&lead);
    header.append(&segbar);
    header.append(&trail);

    let browse = gtk::Box::new(Orientation::Vertical, 0);
    browse.append(&header);
    browse.append(&browse_stack);

    // --- Empty hint ---
    let empty = gtk::Box::new(Orientation::Vertical, 10);
    empty.set_valign(gtk::Align::Center);
    empty.set_vexpand(true);
    let ei = gtk::Image::from_icon_name("folder-music-symbolic");
    ei.set_pixel_size(64);
    ei.add_css_class("dim-label");
    let et = gtk::Label::new(Some("Your Library Is Empty"));
    et.add_css_class("title-2");
    let es = gtk::Label::new(Some("Use the menu → Scan Music Folder to index your music."));
    es.add_css_class("dim-label");
    empty.append(&ei);
    empty.append(&et);
    empty.append(&es);

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
        seg,
        sort_btn,
        sort_label,
        sort_opts,
    }
}

fn build_search() -> (gtk::Widget, gtk::SearchEntry, gtk::Box, Vec<(Filter, gtk::ToggleButton)>) {
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
    let results_scroller = wrap_scroller(&results);

    let page = gtk::Box::new(Orientation::Vertical, 0);
    page.append(&top);
    page.append(&results_scroller);

    (page.upcast(), entry, results, buttons)
}

fn build_mini() -> (gtk::Revealer, gtk::Box, gtk::Label, gtk::Label, gtk::Button, gtk::ProgressBar) {
    let art = gtk::Box::new(Orientation::Vertical, 0);
    art.set_size_request(ART_MINI, ART_MINI);
    let title = gtk::Label::new(None);
    title.add_css_class("heading");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let artist = gtk::Label::new(None);
    artist.add_css_class("dim-label");
    artist.add_css_class("caption");
    artist.set_xalign(0.0);
    artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let labels = gtk::Box::new(Orientation::Vertical, 0);
    labels.set_hexpand(true);
    labels.set_valign(gtk::Align::Center);
    labels.append(&title);
    labels.append(&artist);
    let play = circle("media-playback-start-symbolic", 40, "Play");

    let row = gtk::Box::new(Orientation::Horizontal, 11);
    row.set_margin_top(8);
    row.set_margin_bottom(8);
    row.set_margin_start(12);
    row.set_margin_end(12);
    row.append(&art);
    row.append(&labels);
    row.append(&play);

    let progress = gtk::ProgressBar::new();
    progress.add_css_class("mini-progress");
    progress.set_fraction(0.0);

    let bar = gtk::Box::new(Orientation::Vertical, 0);
    bar.add_css_class("toolbar");
    bar.append(&row);
    bar.append(&progress);

    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideUp);
    revealer.set_child(Some(&bar));
    revealer.set_reveal_child(false);

    (revealer, art, title, artist, play, progress)
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn wire(
    state: &SharedState,
    ui: &SharedUi,
    open_btn: &gtk::Button,
    menu_btn: &gtk::MenuButton,
    mp_play: &gtk::Button,
    np_play: &gtk::Button,
    np_shuffle: &gtk::Button,
    np_repeat: &gtk::Button,
    np_prev: &gtk::Button,
    np_next: &gtk::Button,
    np_rewind: &gtk::Button,
    np_fwd: &gtk::Button,
) {
    // open file → play immediately
    {
        let (state, ui) = (state.clone(), ui.clone());
        open_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Choose an audio file").build();
            let (state, ui) = (state.clone(), ui.clone());
            let win = ui.window.clone();
            dialog.open(Some(&win), gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        let t = quick_track(&path);
                        play_list(&state, &ui, vec![t], 0);
                    }
                }
            });
        });
    }

    // menu: scan / rescan
    {
        let popover = gtk::Popover::new();
        let menu = gtk::Box::new(Orientation::Vertical, 2);
        let scan_btn = flat_menu_item("folder-symbolic", "Scan Music Folder…");
        let rescan_btn = flat_menu_item("view-refresh-symbolic", "Rescan Library");
        let settings_btn = flat_menu_item("emblem-system-symbolic", "Settings…");
        menu.append(&scan_btn);
        menu.append(&rescan_btn);
        menu.append(&gtk::Separator::new(Orientation::Horizontal));
        menu.append(&settings_btn);
        popover.set_child(Some(&menu));
        menu_btn.set_popover(Some(&popover));

        {
            let (state, ui, popover) = (state.clone(), ui.clone(), popover.clone());
            settings_btn.connect_clicked(move |_| {
                popover.popdown();
                open_settings(&state, &ui);
            });
        }

        {
            let (state, ui, popover) = (state.clone(), ui.clone(), popover.clone());
            scan_btn.connect_clicked(move |_| {
                popover.popdown();
                let dialog = gtk::FileDialog::builder().title("Choose your music folder").build();
                let (state, ui) = (state.clone(), ui.clone());
                let win = ui.window.clone();
                dialog.select_folder(Some(&win), gio::Cancellable::NONE, move |res| {
                    if let Ok(folder) = res {
                        if let Some(path) = folder.path() {
                            state.borrow_mut().music_dir = Some(path.clone());
                            start_scan(&state, &ui, path, false);
                        }
                    }
                });
            });
        }
        {
            let (state, ui, popover) = (state.clone(), ui.clone(), popover.clone());
            rescan_btn.connect_clicked(move |_| {
                popover.popdown();
                let dir = state.borrow().music_dir.clone();
                match dir {
                    // Force re-extraction so covers/tags refresh even when files
                    // are byte-unchanged on disk.
                    Some(d) => start_scan(&state, &ui, d, true),
                    None => ui.toast("Pick a music folder first (Scan Music Folder…)"),
                }
            });
        }
    }

    // play / pause (mirrored on both buttons)
    for btn in [mp_play, np_play] {
        let (state, ui) = (state.clone(), ui.clone());
        btn.connect_clicked(move |_| toggle_play(&state, &ui));
    }
    {
        let (state, ui) = (state.clone(), ui.clone());
        np_prev.connect_clicked(move |_| prev_track(&state, &ui));
    }
    {
        let (state, ui) = (state.clone(), ui.clone());
        np_next.connect_clicked(move |_| advance(&state, &ui, true));
    }
    {
        let (state, ui) = (state.clone(), ui.clone());
        np_rewind.connect_clicked(move |_| seek_by(&state, &ui, -10_000));
    }
    {
        let (state, ui) = (state.clone(), ui.clone());
        np_fwd.connect_clicked(move |_| seek_by(&state, &ui, 10_000));
    }
    {
        let state = state.clone();
        np_shuffle.connect_clicked(move |b| {
            let on = !state.borrow().shuffle;
            state.borrow_mut().shuffle = on;
            toggle_accent(b, on);
        });
    }
    {
        let state = state.clone();
        np_repeat.connect_clicked(move |b| {
            let on = !state.borrow().repeat;
            state.borrow_mut().repeat = on;
            toggle_accent(b, on);
        });
    }

    // interactive seek. `change-value` fires only on *user* input (drag, click,
    // arrow/scroll) — never on our programmatic `set_value` in `on_position`,
    // which would otherwise feed back as a seek. A `GestureClick` on a `Scale` is
    // unreliable (the Scale claims the drag sequence, so `released` is cancelled
    // and the seek never commits — the bug we're fixing). Each move marks
    // `seeking` (freezing `on_position`), updates the elapsed readout live, and
    // arms a debounce: only the latest generation commits the engine seek, so a
    // continuous drag results in one seek when the handle settles.
    {
        let (state, ui) = (state.clone(), ui.clone());
        ui.np_seek.clone().connect_change_value(move |_, _, value| {
            let frac = value.clamp(0.0, 1.0);
            ui.seeking.set(true);
            let total_ms = current_total_ms(&state);
            if total_ms > 0 {
                ui.np_elapsed.set_label(&mmss((frac * total_ms as f64 / 1000.0) as u64));
            }
            let gen = ui.seek_gen.get().wrapping_add(1);
            ui.seek_gen.set(gen);
            let (state, ui) = (state.clone(), ui.clone());
            glib::timeout_add_local_once(Duration::from_millis(180), move || {
                if ui.seek_gen.get() != gen {
                    return; // a newer move superseded this one
                }
                ui.seeking.set(false);
                seek_to_fraction(&state, &ui, frac);
            });
            glib::Propagation::Proceed
        });
    }

    // mini-player tap → Playing page
    {
        let ui2 = ui.clone();
        let click = gtk::GestureClick::new();
        click.connect_released(move |_, _, _, _| ui2.stack.set_visible_child_name("playing"));
        ui.mini.add_controller(click);
    }

    // hide the mini bar on the Playing page
    {
        let (state, ui) = (state.clone(), ui.clone());
        let stack = ui.stack.clone();
        stack.connect_visible_child_name_notify(move |_| update_mini(&state, &ui));
    }

    // search: typed query + filter chips. Debounced; the query runs on the
    // worker thread and results are rendered by the search-results pump.
    {
        let ui = ui.clone();
        let entry = ui.search_entry.clone();
        entry.connect_search_changed(move |_| kick_search(&ui));
    }
    for (f, btn) in &ui.filter_buttons {
        let (ui, f) = (ui.clone(), *f);
        btn.connect_toggled(move |b| {
            if !b.is_active() {
                return;
            }
            *ui.filter.borrow_mut() = f;
            for (of, ob) in &ui.filter_buttons {
                if *of != f {
                    ob.set_active(false);
                }
            }
            kick_search(&ui);
        });
    }
    // (Album activation is wired by `refresh_library` on the GridView itself.)
}

/// Wire the Albums/Artists/Folders/Tracks segmented switcher (radio behaviour).
fn wire_segmented(state: &SharedState, ui: &SharedUi, seg: &[(&'static str, gtk::ToggleButton)]) {
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
fn wire_sort(
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

// ---------------------------------------------------------------------------
// Playback control (queue driven by the app; engine = play/enqueue/stop)
// ---------------------------------------------------------------------------

fn play_list(state: &SharedState, ui: &SharedUi, tracks: Vec<Track>, start: usize) {
    if tracks.is_empty() {
        return;
    }
    {
        let mut s = state.borrow_mut();
        s.queue = tracks;
        s.current = Some(start);
        s.playing = true;
        s.resume_to = None; // an explicit new selection cancels a pending resume
    }
    start_current(state, ui);
    ui.stack.set_visible_child_name("playing");
}

fn start_current(state: &SharedState, ui: &SharedUi) {
    let track = {
        let s = state.borrow();
        s.current.and_then(|i| s.queue.get(i).cloned())
    };
    let Some(track) = track else { return };
    show_track(state, ui, &track);
    update_now_playing_empty(ui, false);
    set_play_icon(ui, true);
    {
        let mut s = state.borrow_mut();
        s.paused = false;
        // A .cue track decodes a sub-range of its source file; everything else is
        // a whole-file play.
        match track.cue_range() {
            Some((start, end)) => s.player.play_range(track.source_path.clone(), start, end),
            None => s.player.play(track.path.clone()),
        }
        // Log to recently-played history (indexed tracks only).
        if track.id > 0 {
            let _ = s.library.record_play(track.id);
        }
    }
    update_mini(state, ui);
    refresh_queue(state, ui);
}

fn toggle_play(state: &SharedState, ui: &SharedUi) {
    let (playing, paused, has_current, has_queue) = {
        let s = state.borrow();
        (s.playing, s.paused, s.current.is_some(), !s.queue.is_empty())
    };
    if playing {
        // Real pause: hold output at the device, keep the track loaded.
        state.borrow().player.pause();
        let mut s = state.borrow_mut();
        s.playing = false;
        s.paused = true;
        drop(s);
        set_play_icon(ui, false);
    } else if paused {
        // Resume the held track (do not restart it).
        state.borrow().player.resume();
        let mut s = state.borrow_mut();
        s.playing = true;
        s.paused = false;
        drop(s);
        set_play_icon(ui, true);
    } else if has_current {
        state.borrow_mut().playing = true;
        start_current(state, ui);
    } else if has_queue {
        let mut s = state.borrow_mut();
        s.current = Some(0);
        s.playing = true;
        drop(s);
        start_current(state, ui);
    }
}

/// Total duration (ms) of the currently-selected queue track, or 0 if unknown.
fn current_total_ms(state: &SharedState) -> u64 {
    let s = state.borrow();
    s.current
        .and_then(|i| s.queue.get(i))
        .and_then(|t| t.duration_ms)
        .unwrap_or(0)
}

/// Seek the current track to `frac` (0..1) of its duration.
fn seek_to_fraction(state: &SharedState, ui: &SharedUi, frac: f64) {
    let total_ms = current_total_ms(state);
    if total_ms == 0 {
        return;
    }
    let pos = Duration::from_millis((total_ms as f64 * frac) as u64);
    {
        let mut s = state.borrow_mut();
        s.player.seek(pos);
        // Seek (re)starts playback in the engine — reflect that in the UI.
        s.playing = true;
        s.paused = false;
    }
    set_play_icon(ui, true);
}

/// Seek relative to the current position by `delta_ms` (negative = back),
/// clamped to the track. Used by the −10s / +10s transport buttons.
fn seek_by(state: &SharedState, ui: &SharedUi, delta_ms: i64) {
    let total_ms = current_total_ms(state);
    if total_ms == 0 {
        return;
    }
    let target = (state.borrow().last_pos_ms as i64 + delta_ms).clamp(0, total_ms as i64) as u64;
    {
        let mut s = state.borrow_mut();
        s.player.seek(Duration::from_millis(target));
        // Seek (re)starts playback in the engine — reflect that in the UI.
        s.playing = true;
        s.paused = false;
        s.last_pos_ms = target;
    }
    set_play_icon(ui, true);
    // Reflect the jump immediately (don't wait for the next Position event).
    ui.seeking.set(false);
    ui.np_elapsed.set_label(&mmss(target / 1000));
    let frac = target as f64 / total_ms as f64;
    ui.np_seek.set_value(frac);
    ui.mp_progress.set_fraction(frac);
}

fn advance(state: &SharedState, ui: &SharedUi, user: bool) {
    let next = {
        let s = state.borrow();
        let len = s.queue.len();
        if len == 0 {
            None
        } else if s.shuffle {
            Some(pseudo_random(len))
        } else {
            match s.current {
                Some(i) if i + 1 < len => Some(i + 1),
                _ if s.repeat => Some(0),
                _ => None,
            }
        }
    };
    match next {
        Some(i) => {
            state.borrow_mut().current = Some(i);
            state.borrow_mut().playing = true;
            start_current(state, ui);
        }
        None => {
            state.borrow_mut().playing = false;
            set_play_icon(ui, false);
            if !user {
                ui.title.set_subtitle("■ finished");
            }
        }
    }
}

fn prev_track(state: &SharedState, ui: &SharedUi) {
    let i = {
        let s = state.borrow();
        match s.current {
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
            None => return,
        }
    };
    state.borrow_mut().current = Some(i);
    state.borrow_mut().playing = true;
    start_current(state, ui);
}

/// Open the detail page for the album at `index` in the cached album list.
fn open_album_detail(state: &SharedState, ui: &SharedUi, index: usize) {
    let (album, tracks) = {
        let s = state.borrow();
        let Some(al) = s.albums.get(index).cloned() else { return };
        let tracks = s
            .library
            .album_tracks(&al.album, al.album_artist.as_deref())
            .unwrap_or_default();
        (al, tracks)
    };
    let page = build_album_detail(state, ui, &album, tracks);
    ui.nav.push(&page);
}

/// Build an album-detail navigation page: a hero header (art + titles), the
/// Play / Shuffle / Add-to-queue actions, and the track list.
fn build_album_detail(
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
        let (state, ui, all) = (state.clone(), ui.clone(), tracks.clone());
        let (s2, u2, tc) = (state.clone(), ui.clone(), tr.clone());
        let cache = ui.art.clone();
        let n = tr.track_no.map(|n| format!("{n}.  ")).unwrap_or_default();
        let row = row_widget(
            &cache,
            tr.art_hash.as_deref(),
            &format!("{n}{}", tr.display_title()),
            &tr.subtitle(),
            Some(&fmt_dur_ms(tr.duration_ms.unwrap_or(0))),
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

    adw::NavigationPage::new(&wrap_scroller(&content), &album.album)
}

/// The track currently loaded in the queue, if any.
fn current_track(state: &SharedState) -> Option<Track> {
    let s = state.borrow();
    s.current.and_then(|i| s.queue.get(i).cloned())
}

/// Push the detail page for `artist` and reveal the Library tab (works from the
/// browse list, search results, or the now-playing nav buttons).
fn open_artist_detail(state: &SharedState, ui: &SharedUi, artist: &str) {
    let albums = state.borrow().library.artist_albums(artist).unwrap_or_default();
    let page = build_artist_detail(state, ui, artist, albums);
    ui.nav.push(&page);
    ui.stack.set_visible_child_name("library");
}

/// Push the album detail page for `(album, album_artist)`, looking the `Album` up
/// in the cached list and synthesising it from the tracks if it isn't cached.
fn open_album_for(state: &SharedState, ui: &SharedUi, album: &str, album_artist: Option<&str>) {
    let found = {
        let s = state.borrow();
        s.albums
            .iter()
            .find(|a| a.album == album && a.album_artist.as_deref() == album_artist)
            .cloned()
    };
    let tracks = state
        .borrow()
        .library
        .album_tracks(album, album_artist)
        .unwrap_or_default();
    if tracks.is_empty() {
        ui.toast("No tracks for this album");
        return;
    }
    let al = found.unwrap_or_else(|| Album {
        album: album.to_string(),
        album_artist: album_artist.map(|s| s.to_string()),
        year: tracks.iter().find_map(|t| t.year),
        track_count: tracks.len() as u32,
        total_ms: tracks.iter().filter_map(|t| t.duration_ms).sum(),
        art_hash: tracks.iter().find_map(|t| t.art_hash.clone()),
    });
    let page = build_album_detail(state, ui, &al, tracks);
    ui.nav.push(&page);
    ui.stack.set_visible_child_name("library");
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
    let avatar = gtk::Box::new(Orientation::Vertical, 0);
    avatar.set_size_request(132, 132);
    avatar.add_css_class("art-lg");
    let img = gtk::Image::from_icon_name("avatar-default-symbolic");
    img.set_pixel_size(72);
    img.set_hexpand(true);
    img.set_vexpand(true);
    img.add_css_class("dim-label");
    avatar.append(&img);
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
        let (state, ui, name) = (state.clone(), ui.clone(), artist.to_string());
        shuffle_btn.connect_clicked(move |_| {
            let mut v = state.borrow().library.artist_tracks(&name).unwrap_or_default();
            shuffle_vec(&mut v);
            play_list(&state, &ui, v, 0);
        });
    }
    {
        let (state, ui, name) = (state.clone(), ui.clone(), artist.to_string());
        add_btn.connect_clicked(move |_| {
            let v = state.borrow().library.artist_tracks(&name).unwrap_or_default();
            enqueue_tracks(&state, &ui, v);
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
            Some(("media-playback-start-symbolic", "Play album")),
            move || open_album_for(&s1, &u1, &a1.album, a1.album_artist.as_deref()),
            move || {
                let tracks = s2
                    .borrow()
                    .library
                    .album_tracks(&a2.album, a2.album_artist.as_deref())
                    .unwrap_or_default();
                play_list(&s2, &u2, tracks, 0);
            },
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

    adw::NavigationPage::new(&wrap_scroller(&content), artist)
}

fn enqueue_track(state: &SharedState, ui: &SharedUi, track: Track) {
    state.borrow_mut().queue.push(track);
    refresh_queue(state, ui);
    ui.toast("Added to queue");
}

fn enqueue_tracks(state: &SharedState, ui: &SharedUi, tracks: Vec<Track>) {
    let n = tracks.len();
    state.borrow_mut().queue.extend(tracks);
    refresh_queue(state, ui);
    ui.toast(&format!("Added {n} track{} to queue", if n == 1 { "" } else { "s" }));
}

/// In-place time-seeded shuffle (no rng dependency; fine for playback order).
fn shuffle_vec<T>(v: &mut [T]) {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B9)
        | 1;
    for i in (1..v.len()).rev() {
        // xorshift step
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

fn refresh_queue(state: &SharedState, ui: &SharedUi) {
    while let Some(c) = ui.queue_box.first_child() {
        ui.queue_box.remove(&c);
    }
    let (queue, current) = {
        let s = state.borrow();
        (s.queue.clone(), s.current)
    };

    // Up Next = the tracks after the current one (the playing track lives in the
    // mini-player / Playing page, not in this list). Indices are kept absolute so
    // jump/remove still address the real queue slot.
    let start = current.map(|c| c + 1).unwrap_or(0);
    let upcoming: Vec<usize> = (start..queue.len()).collect();
    ui.queue_title.set_text(&if upcoming.is_empty() {
        "Up Next".to_string()
    } else {
        format!("Up Next · {}", upcoming.len())
    });

    if upcoming.is_empty() {
        let l = gtk::Label::new(Some("Nothing queued — play an album or add tracks from search."));
        l.add_css_class("dim-label");
        l.set_wrap(true);
        l.set_margin_top(16);
        ui.queue_box.append(&l);
    } else {
        let lb = boxed_list();
        for i in upcoming {
            let t = &queue[i];
            let (s1, u1) = (state.clone(), ui.clone());
            let (s2, u2) = (state.clone(), ui.clone());
            let row = row_widget(
                &ui.art,
                t.art_hash.as_deref(),
                &t.display_title(),
                &t.subtitle(),
                None,
                Some(("list-remove-symbolic", "Remove from queue")),
                move || jump_to(&s1, &u1, i),
                move || remove_from_queue(&s2, &u2, i),
            );
            lb.append(&row);
        }
        ui.queue_box.append(&lb);
    }

    // Recently played (global history): tap to play now, ＋ to add to queue.
    let recent = state.borrow().library.recent_plays(15).unwrap_or_default();
    if !recent.is_empty() {
        let header = section_label("Recently Played");
        header.set_margin_top(16);
        ui.queue_box.append(&header);
        let lb = boxed_list();
        for t in &recent {
            let (s1, u1, t1) = (state.clone(), ui.clone(), t.clone());
            let (s2, u2, t2) = (state.clone(), ui.clone(), t.clone());
            let row = row_widget(
                &ui.art,
                t.art_hash.as_deref(),
                &t.display_title(),
                &t.subtitle(),
                Some(&fmt_dur_ms(t.duration_ms.unwrap_or(0))),
                Some(("list-add-symbolic", "Add to queue")),
                move || play_track_now(&s1, &u1, t1.clone()),
                move || enqueue_track(&s2, &u2, t2.clone()),
            );
            lb.append(&row);
        }
        ui.queue_box.append(&lb);
    }
}

/// Append `track` to the queue and start playing it immediately.
fn play_track_now(state: &SharedState, ui: &SharedUi, track: Track) {
    let i = {
        let mut s = state.borrow_mut();
        s.queue.push(track);
        s.queue.len() - 1
    };
    jump_to(state, ui, i);
}

/// Stop playback and empty the queue.
fn clear_queue(state: &SharedState, ui: &SharedUi) {
    {
        let mut s = state.borrow_mut();
        s.player.stop();
        s.queue.clear();
        s.current = None;
        s.playing = false;
        s.paused = false;
    }
    ui.title.set_subtitle("■ idle");
    set_play_icon(ui, false);
    update_now_playing_empty(ui, true);
    update_mini(state, ui);
    refresh_queue(state, ui);
}

fn jump_to(state: &SharedState, ui: &SharedUi, i: usize) {
    {
        let mut s = state.borrow_mut();
        if i >= s.queue.len() {
            return;
        }
        s.current = Some(i);
        s.playing = true;
    }
    start_current(state, ui);
    ui.stack.set_visible_child_name("playing");
}

fn remove_from_queue(state: &SharedState, ui: &SharedUi, i: usize) {
    {
        let mut s = state.borrow_mut();
        if i >= s.queue.len() {
            return;
        }
        s.queue.remove(i);
        s.current = match s.current {
            Some(c) if c == i => {
                if s.queue.is_empty() {
                    None
                } else {
                    Some(c.min(s.queue.len() - 1))
                }
            }
            Some(c) if c > i => Some(c - 1),
            other => other,
        };
    }
    refresh_queue(state, ui);
}

// ---------------------------------------------------------------------------
// Engine event handlers
// ---------------------------------------------------------------------------

fn on_started(state: &SharedState, ui: &SharedUi, spec: StreamSpec) {
    {
        let mut s = state.borrow_mut();
        s.playing = true;
        // The engine's StreamSpec is the authoritative wire rate. Backfill the
        // current track's sample rate from it when the library/tag metadata
        // didn't carry one, so position (frames → seconds) is always computable.
        if let Some(i) = s.current {
            if let Some(t) = s.queue.get_mut(i) {
                if t.sample_rate.is_none() {
                    t.sample_rate = Some(spec.rate);
                }
            }
        }
        // Restored session: once the track is open, seek it to the saved
        // position (doing this on Started, not right after play(), so the
        // decoder is actually loaded when the seek lands).
        if let Some(d) = s.resume_to.take() {
            s.player.seek(d);
        }
    }
    set_play_icon(ui, true);
    // authoritative wire format from the engine
    let chip = format!(
        "{}-bit · {} · {}",
        spec.source_bits,
        fmt_khz(spec.rate),
        spec.fmt.label()
    );
    fill(&ui.np_format, &format_chip(&chip));
}

fn on_position(state: &SharedState, ui: &SharedUi, frames: u64) {
    let (rate, total_ms) = {
        let s = state.borrow();
        let rate = s
            .current
            .and_then(|i| s.queue.get(i))
            .and_then(|t| t.sample_rate)
            .unwrap_or(0);
        let total = s
            .current
            .and_then(|i| s.queue.get(i))
            .and_then(|t| t.duration_ms)
            .unwrap_or(0);
        (rate, total)
    };
    if rate == 0 {
        return;
    }
    state.borrow_mut().last_pos_ms = frames * 1000 / rate as u64;
    // Don't fight the user while they drag the handle.
    if ui.seeking.get() {
        return;
    }
    let secs = frames / rate as u64;
    ui.np_elapsed.set_label(&mmss(secs));
    if total_ms > 0 {
        let frac = (secs as f64 / (total_ms as f64 / 1000.0)).clamp(0.0, 1.0);
        ui.np_seek.set_value(frac);
        ui.mp_progress.set_fraction(frac);
    }
}

fn on_ended(state: &SharedState, ui: &SharedUi) {
    advance(state, ui, false);
}

// ---------------------------------------------------------------------------
// View refresh
// ---------------------------------------------------------------------------

fn refresh_library(state: &SharedState, ui: &SharedUi) {
    let albums = state.borrow().library.albums(ui.sort.get()).unwrap_or_default();
    let n_tracks = state
        .borrow()
        .library
        .stats()
        .map(|s| s.tracks)
        .unwrap_or(0);

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

    // Albums grid: a virtualised GridView. Activation opens the album detail by
    // its position in the cached album list (kept in sync via `queue_albums`).
    let grid = grid_view(
        albums.clone(),
        3,
        {
            let cache = ui.art.clone();
            move |al: &Album| album_cell(&cache, al).upcast()
        },
        {
            let (state, ui) = (state.clone(), ui.clone());
            move |_albums: &[Album], pos| open_album_detail(&state, &ui, pos)
        },
    );
    ui.albums_scroller.set_child(Some(&grid));
    build_az_index(ui, &albums, &grid);

    // remember albums for activation lookup
    state.borrow_mut().queue_albums(albums);

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

/// Populate and reveal a browse tab. Albums are filled by [`refresh_library`];
/// Artists/Folders/Tracks are built here on demand as virtualised `ListView`s.
fn show_browse_tab(state: &SharedState, ui: &SharedUi, name: &str) {
    ui.browse_stack.set_visible_child_name(name);
    match name {
        "artists" => {
            let artists = state.borrow().library.artists().unwrap_or_default();
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
        "folders" => {
            let folders = state.borrow().library.folders().unwrap_or_default();
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
        "tracks" => {
            let tracks = state
                .borrow()
                .library
                .tracks(ui.sort.get())
                .unwrap_or_default();
            let lv = list_view(
                tracks,
                {
                    let (state, ui) = (state.clone(), ui.clone());
                    move |t: &Track| {
                        let (s2, u2, tc) = (state.clone(), ui.clone(), t.clone());
                        row_inner(
                            &ui.art,
                            t.art_hash.as_deref(),
                            &t.display_title(),
                            &t.subtitle(),
                            Some(&t.format_spec()),
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
            );
            ui.tracks_scroller.set_child(Some(&lv));
        }
        _ => {}
    }
}

fn play_artist(state: &SharedState, ui: &SharedUi, artist: &str) {
    let tracks = state.borrow().library.artist_tracks(artist).unwrap_or_default();
    if tracks.is_empty() {
        ui.toast("No tracks for that artist");
        return;
    }
    play_list(state, ui, tracks, 0);
}

fn play_folder(state: &SharedState, ui: &SharedUi, folder: &str) {
    let tracks = state.borrow().library.folder_tracks(folder).unwrap_or_default();
    if tracks.is_empty() {
        ui.toast("No tracks in that folder");
        return;
    }
    play_list(state, ui, tracks, 0);
}

fn build_az_index(ui: &SharedUi, albums: &[Album], grid: &gtk::GridView) {
    let mut last = '\0';
    for (i, al) in albums.iter().enumerate() {
        let c = al
            .album
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or('#');
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

/// A request to / result from the background search worker.
enum SearchMsg {
    Query { seq: u64, text: String, filter: Filter },
    /// Rebuild the in-memory fuzzy index (after a scan changed the library).
    Reindex,
}

struct SearchHits {
    seq: u64,
    results: SearchResults,
}

/// Spawn the search worker: it owns its own read-only library connection and an
/// in-memory [`SearchIndex`], answers `Query` messages off the main thread, and
/// rebuilds the index on `Reindex`. Bursts that pile up while it is busy are
/// coalesced to the latest query (older keystrokes never render — latest-wins).
fn spawn_search_worker(
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
fn kick_search(ui: &SharedUi) {
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

fn clear_search_results(ui: &SharedUi) {
    while let Some(c) = ui.search_results.first_child() {
        ui.search_results.remove(&c);
    }
}

/// Render the unified grouped results (Artists · Albums · Tracks · Folders).
fn render_search_results(state: &SharedState, ui: &SharedUi, results: SearchResults) {
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

// ---------------------------------------------------------------------------
// Now-playing display helpers
// ---------------------------------------------------------------------------

fn show_track(_state: &SharedState, ui: &SharedUi, track: &Track) {
    ui.title.set_subtitle(&format!("▶ {}", track.display_title()));
    ui.np_title.set_label(&track.display_title());
    ui.np_subtitle.set_label(&track.subtitle());
    ui.np_total
        .set_label(&fmt_dur_ms(track.duration_ms.unwrap_or(0)));
    ui.np_elapsed.set_label("0:00");
    ui.np_seek.set_value(0.0);
    ui.mp_progress.set_fraction(0.0);
    fill(&ui.np_art, &art_widget(&ui.art, track.art_hash.as_deref(), ART_HERO, true));
    fill(&ui.mp_art, &art_widget(&ui.art, track.art_hash.as_deref(), ART_MINI, false));
    ui.mp_title.set_label(&track.display_title());
    ui.mp_artist.set_label(&track.subtitle());
    fill(&ui.np_format, &format_chip(&track.signal_spec()));
    let has_artist = track
        .album_artist
        .as_deref()
        .or(track.artist.as_deref())
        .is_some_and(|a| !a.is_empty());
    let has_album = track.album.as_deref().is_some_and(|a| !a.is_empty());
    ui.np_goto_artist.set_sensitive(has_artist);
    ui.np_goto_album.set_sensitive(has_album);
}

fn update_now_playing_empty(ui: &SharedUi, empty: bool) {
    ui.np_stack
        .set_visible_child_name(if empty { "empty" } else { "content" });
}

fn update_mini(state: &SharedState, ui: &SharedUi) {
    let has = state.borrow().current.is_some();
    let on_playing = ui.stack.visible_child_name().as_deref() == Some("playing");
    ui.mini.set_reveal_child(has && !on_playing);
}

fn set_play_icon(ui: &SharedUi, playing: bool) {
    let icon = if playing {
        "media-playback-pause-symbolic"
    } else {
        "media-playback-start-symbolic"
    };
    ui.np_play.set_icon_name(icon);
    ui.mp_play.set_icon_name(icon);
}

// ---------------------------------------------------------------------------
// Scanning (worker thread)
// ---------------------------------------------------------------------------

enum ScanMsg {
    Progress(u64, u64),
    Done(Result<player_library::ScanStats, String>),
}

/// Scan `root` into the library on a worker thread. `force` re-extracts every
/// file even when unchanged — used by "Rescan Library" so newly-supported data
/// (e.g. folder-sidecar covers) is backfilled into an already-indexed library.
fn start_scan(state: &SharedState, ui: &SharedUi, root: PathBuf, force: bool) {
    ui.toast(&format!("Scanning {}…", root.display()));
    let (db, art) = {
        let s = state.borrow();
        (s.db_path.clone(), s.art_dir.clone())
    };
    let (tx, rx) = async_channel::unbounded::<ScanMsg>();

    std::thread::spawn(move || match Library::open(&db, &art) {
        Ok(lib) => {
            let tx2 = tx.clone();
            let res = lib.scan_with_progress(&root, force, move |p| {
                let _ = tx2.send_blocking(ScanMsg::Progress(p.seen, p.total));
            });
            let _ = tx.send_blocking(ScanMsg::Done(res.map_err(|e| e.to_string())));
        }
        Err(e) => {
            let _ = tx.send_blocking(ScanMsg::Done(Err(e.to_string())));
        }
    });

    let (state, ui) = (state.clone(), ui.clone());
    glib::spawn_future_local(async move {
        while let Ok(msg) = rx.recv().await {
            match msg {
                ScanMsg::Progress(seen, total) => {
                    ui.title.set_subtitle(&format!("Scanning {seen}/{total}…"));
                }
                ScanMsg::Done(Ok(s)) => {
                    let _ = ui.search_tx.try_send(SearchMsg::Reindex);
                    refresh_library(&state, &ui);
                    ui.title.set_subtitle("■ idle");
                    ui.toast(&format!(
                        "Indexed · +{} ~{} −{} ({} tracks)",
                        s.added,
                        s.updated + s.moved,
                        s.removed,
                        s.total_seen()
                    ));
                    break;
                }
                ScanMsg::Done(Err(e)) => {
                    ui.toast(&format!("Scan failed: {e}"));
                    break;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Device / Settings
// ---------------------------------------------------------------------------

/// Replace the running engine with one bound to `device`, reusing the existing
/// event channel so the event pump keeps working. Persists the choice.
fn respawn_player(state: &SharedState, device: String) {
    let ev_tx = state.borrow().ev_tx.clone();
    let new_player = {
        let ev_tx = ev_tx.clone();
        Player::spawn(device.clone(), move |ev| {
            let _ = ev_tx.send_blocking(ev);
        })
    };
    let mut s = state.borrow_mut();
    s.player = new_player; // dropping the old player joins its threads
    let _ = s.library.set_meta("device", &device);
    s.device = device;
    s.playing = false;
    s.paused = false;
}

/// Open the preferences dialog: output device, library folder + live watch, theme.
fn open_settings(state: &SharedState, ui: &SharedUi) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Settings");
    let page = adw::PreferencesPage::new();

    // --- Output device ---
    let out = adw::PreferencesGroup::new();
    out.set_title("Output");
    out.set_description(Some("Bit-perfect hardware devices only — no resampling layers."));
    let devices = player_core::list_devices();
    let cur_dev = state.borrow().device.clone();
    let model = gtk::StringList::new(&[]);
    let mut sel = 0u32;
    for (i, d) in devices.iter().enumerate() {
        model.append(&format!("{} · {}", d.name, d.id));
        if d.id == cur_dev {
            sel = i as u32;
        }
    }
    let dev_row = adw::ComboRow::new();
    dev_row.set_title("Device");
    dev_row.set_model(Some(&model));
    if devices.is_empty() {
        dev_row.set_subtitle(&cur_dev);
        dev_row.set_sensitive(false);
    } else {
        dev_row.set_selected(sel);
        let ids: Vec<String> = devices.iter().map(|d| d.id.clone()).collect();
        let (state, ui) = (state.clone(), ui.clone());
        dev_row.connect_selected_notify(move |r| {
            if let Some(id) = ids.get(r.selected() as usize) {
                if *id != state.borrow().device {
                    respawn_player(&state, id.clone());
                    ui.toast(&format!("Output → {id}"));
                }
            }
        });
    }
    out.add(&dev_row);
    page.add(&out);

    // --- Library ---
    let libg = adw::PreferencesGroup::new();
    libg.set_title("Library");
    let folder_row = adw::ActionRow::new();
    folder_row.set_title("Music Folder");
    let cur_dir = state
        .borrow()
        .music_dir
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "Not set".into());
    folder_row.set_subtitle(&cur_dir);
    let choose = gtk::Button::from_icon_name("folder-open-symbolic");
    choose.add_css_class("flat");
    choose.set_valign(gtk::Align::Center);
    choose.set_tooltip_text(Some("Choose and scan a music folder"));
    {
        let (state, ui, folder_row) = (state.clone(), ui.clone(), folder_row.clone());
        let win = ui.window.clone();
        choose.connect_clicked(move |_| {
            let chooser = gtk::FileDialog::builder().title("Choose your music folder").build();
            let (state, ui, folder_row) = (state.clone(), ui.clone(), folder_row.clone());
            chooser.select_folder(Some(&win), gio::Cancellable::NONE, move |res| {
                if let Ok(folder) = res {
                    if let Some(path) = folder.path() {
                        folder_row.set_subtitle(&path.display().to_string());
                        {
                            let mut s = state.borrow_mut();
                            let _ = s.library.set_meta("music_dir", &path.to_string_lossy());
                            s.music_dir = Some(path.clone());
                        }
                        start_scan(&state, &ui, path, false);
                    }
                }
            });
        });
    }
    folder_row.add_suffix(&choose);
    libg.add(&folder_row);

    let watch_row = adw::SwitchRow::new();
    watch_row.set_title("Watch for Changes");
    watch_row.set_subtitle("Re-index automatically when the folder changes");
    watch_row.set_active(state.borrow().watcher.is_some());
    {
        let (state, ui) = (state.clone(), ui.clone());
        watch_row.connect_active_notify(move |r| set_watch(&state, &ui, r.is_active()));
    }
    libg.add(&watch_row);
    page.add(&libg);

    // --- Appearance ---
    let appg = adw::PreferencesGroup::new();
    appg.set_title("Appearance");
    let theme_row = adw::ComboRow::new();
    theme_row.set_title("Theme");
    let theme_model = gtk::StringList::new(&["System", "Light", "Dark"]);
    theme_row.set_model(Some(&theme_model));
    let cur_theme = state.borrow().library.get_meta("theme").ok().flatten();
    theme_row.set_selected(match cur_theme.as_deref() {
        Some("light") => 1,
        Some("dark") => 2,
        _ => 0,
    });
    {
        let state = state.clone();
        theme_row.connect_selected_notify(move |r| {
            let name = match r.selected() {
                1 => "light",
                2 => "dark",
                _ => "system",
            };
            apply_theme(Some(name));
            let _ = state.borrow().library.set_meta("theme", name);
        });
    }
    appg.add(&theme_row);
    page.add(&appg);

    dialog.add(&page);
    dialog.present(Some(&ui.window));
}

/// Enable or disable live folder-watching. When on, a background thread bridges
/// the `notify` events to the main loop, which debounces and re-scans.
fn set_watch(state: &SharedState, ui: &SharedUi, on: bool) {
    if !on {
        state.borrow_mut().watcher = None; // drops watcher → bridge thread ends
        return;
    }
    let root = match state.borrow().music_dir.clone() {
        Some(r) => r,
        None => {
            ui.toast("Pick a music folder first");
            return;
        }
    };
    match Library::watch(&root) {
        Ok(w) => {
            let rx = w.rx.clone();
            state.borrow_mut().watcher = Some(w);
            let (tx, arx) = async_channel::unbounded::<()>();
            std::thread::spawn(move || {
                while rx.recv().is_ok() {
                    let _ = tx.send_blocking(());
                }
            });
            {
                let (state, ui) = (state.clone(), ui.clone());
                glib::spawn_future_local(async move {
                    while arx.recv().await.is_ok() {
                        // settle, then coalesce the burst
                        glib::timeout_future(Duration::from_millis(800)).await;
                        while arx.try_recv().is_ok() {}
                        let dir = state.borrow().music_dir.clone();
                        if let Some(d) = dir {
                            start_scan(&state, &ui, d, false);
                        }
                    }
                });
            }
            ui.toast("Watching for changes");
        }
        Err(e) => ui.toast(&format!("Watch failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Session + playlist persistence
// ---------------------------------------------------------------------------

/// Persist the current queue, selected index, and position for next launch.
fn save_session(state: &SharedState) {
    let s = state.borrow();
    let paths: Vec<String> = s
        .queue
        .iter()
        .map(|t| t.path.to_string_lossy().into_owned())
        .collect();
    let _ = s.library.set_meta("queue", &paths.join("\n"));
    let _ = s.library.set_meta(
        "current",
        &s.current.map(|i| i.to_string()).unwrap_or_default(),
    );
    let _ = s.library.set_meta("pos_ms", &s.last_pos_ms.to_string());
    if let Some(d) = &s.music_dir {
        let _ = s.library.set_meta("music_dir", &d.to_string_lossy());
    }
}

/// Restore the last session's queue/position. Loads the queue but does not
/// auto-play; pressing play resumes the current track at its saved position.
fn restore_session(state: &SharedState, ui: &SharedUi) {
    let (queue_raw, current_raw, pos_raw) = {
        let s = state.borrow();
        (
            s.library.get_meta("queue").ok().flatten().unwrap_or_default(),
            s.library.get_meta("current").ok().flatten().unwrap_or_default(),
            s.library.get_meta("pos_ms").ok().flatten().unwrap_or_default(),
        )
    };
    if queue_raw.trim().is_empty() {
        return;
    }
    let tracks: Vec<Track> = {
        let s = state.borrow();
        queue_raw
            .lines()
            .filter(|l| !l.is_empty())
            .map(|p| {
                let pb = PathBuf::from(p);
                s.library
                    .track_by_path(&pb)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| quick_track(&pb))
            })
            .collect()
    };
    if tracks.is_empty() {
        return;
    }
    let current = current_raw.parse::<usize>().ok().filter(|&i| i < tracks.len());
    let pos_ms = pos_raw.parse::<u64>().unwrap_or(0);
    {
        let mut s = state.borrow_mut();
        s.queue = tracks;
        s.current = current;
        s.resume_to = (pos_ms > 0).then(|| Duration::from_millis(pos_ms));
    }
    if let Some(i) = current {
        let track = state.borrow().queue.get(i).cloned();
        if let Some(track) = track {
            show_track(state, ui, &track);
            update_now_playing_empty(ui, false);
            set_play_icon(ui, false);
        }
    }
    refresh_queue(state, ui);
    update_mini(state, ui);
}

/// Save the current queue to `path` as an `.m3u` playlist.
fn save_playlist(state: &SharedState, ui: &SharedUi, path: PathBuf) {
    let paths: Vec<String> = state
        .borrow()
        .queue
        .iter()
        .map(|t| t.path.to_string_lossy().into_owned())
        .collect();
    if paths.is_empty() {
        ui.toast("Queue is empty — nothing to save");
        return;
    }
    let path = ensure_ext(path, "m3u");
    let mut body = String::from("#EXTM3U\n");
    for p in &paths {
        body.push_str(p);
        body.push('\n');
    }
    match std::fs::write(&path, body) {
        Ok(()) => ui.toast(&format!("Saved {} track{}", paths.len(), if paths.len() == 1 { "" } else { "s" })),
        Err(e) => ui.toast(&format!("Save failed: {e}")),
    }
}

/// Load an `.m3u` playlist into the queue and start it.
fn load_playlist(state: &SharedState, ui: &SharedUi, path: PathBuf) {
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) => {
            ui.toast(&format!("Load failed: {e}"));
            return;
        }
    };
    let base = path.parent().map(Path::to_path_buf);
    let tracks: Vec<Track> = {
        let s = state.borrow();
        body.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| {
                let pb = resolve_rel(l, base.as_deref());
                s.library
                    .track_by_path(&pb)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| quick_track(&pb))
            })
            .collect()
    };
    if tracks.is_empty() {
        ui.toast("Playlist has no tracks");
        return;
    }
    play_list(state, ui, tracks, 0);
}

/// Ensure `path` ends with `.ext`.
fn ensure_ext(path: PathBuf, ext: &str) -> PathBuf {
    match path.extension() {
        Some(e) if e.eq_ignore_ascii_case(ext) => path,
        _ => path.with_extension(ext),
    }
}

/// Resolve an `.m3u` entry: absolute as-is, else relative to the playlist dir.
fn resolve_rel(entry: &str, base: Option<&Path>) -> PathBuf {
    let p = PathBuf::from(entry);
    if p.is_absolute() {
        return p;
    }
    match base {
        Some(b) => b.join(p),
        None => p,
    }
}

// ---------------------------------------------------------------------------
// Small widget builders
// ---------------------------------------------------------------------------

/// Build a [`Track`] for a file that may not be in the library (Open / playlist
/// / restored session). Delegates to the library extractor so the now-playing
/// view has a real title, duration, and sample rate — without which the seek bar
/// and elapsed clock would never move.
fn quick_track(path: &Path) -> Track {
    player_library::track_from_path(path)
}

fn pseudo_random(len: usize) -> usize {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    n % len.max(1)
}

impl Ui {
    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }
}

impl State {
    /// Stash the album list so flow-box activation can resolve an index.
    fn queue_albums(&mut self, albums: Vec<Album>) {
        self.albums = albums;
    }
}
