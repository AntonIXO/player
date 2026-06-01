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
use gtk::{gio, glib, ContentFit, Orientation, SelectionMode};
use player_core::{Event, Player, StreamSpec};
use player_library::{fmt_dur_ms, fmt_khz, Album, Filter, Library, Track};

const ART_ROW: i32 = 46;
const ART_MINI: i32 = 40;
const ART_HERO: i32 = 220;

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
    np_format: gtk::Box,
    np_stack: gtk::Stack,
    /// True while the user is dragging the seek slider — suppresses programmatic
    /// position updates so the handle does not fight the drag.
    seeking: Cell<bool>,
    /// Monotonic seek-debounce generation: each slider move bumps it and arms a
    /// timeout; only the latest generation actually commits the engine seek.
    seek_gen: Cell<u64>,
    // library
    nav: adw::NavigationView,
    albums_flow: gtk::FlowBox,
    az_box: gtk::Box,
    library_empty: gtk::Widget, // outer Stack: "empty" | "browse"
    browse_stack: gtk::Stack,   // "albums" | "artists" | "folders" | "tracks"
    artists_box: gtk::Box,
    folders_box: gtk::Box,
    tracks_box: gtk::Box,
    // search
    search_entry: gtk::SearchEntry,
    search_results: gtk::Box,
    filter: RefCell<Filter>,
    filter_buttons: Vec<(Filter, gtk::ToggleButton)>,
    // queue
    queue_box: gtk::Box,
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

/// Apply a persisted theme name ("light" / "dark" / anything else = system).
fn apply_theme(theme: Option<&str>) {
    let scheme = match theme {
        Some("light") => adw::ColorScheme::ForceLight,
        Some("dark") => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("style.css"));
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
    let queue_page = wrap_scroller(&{
        let b = gtk::Box::new(Orientation::Vertical, 0);
        b.set_margin_top(14);
        b.set_margin_bottom(20);
        b.set_margin_start(14);
        b.set_margin_end(14);
        let head = gtk::Box::new(Orientation::Horizontal, 6);
        let lbl = section_label("Up Next");
        lbl.set_hexpand(true);
        head.append(&lbl);
        head.append(&pl_load);
        head.append(&pl_save);
        b.append(&head);
        b.append(&queue_box);
        b
    });

    let stack = adw::ViewStack::new();
    add_page(&stack, &lib.nav, "library", "Library", "view-grid-symbolic");
    add_page(&stack, &np_page, "playing", "Playing", "media-playback-start-symbolic");
    add_page(&stack, &search_page, "search", "Search", "system-search-symbolic");
    add_page(&stack, &queue_page, "lists", "Lists", "view-list-symbolic");
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
        np_format: np.format,
        np_stack: np.stack.clone(),
        seeking: Cell::new(false),
        seek_gen: Cell::new(0),
        nav: lib.nav.clone(),
        albums_flow: lib.albums_flow.clone(),
        az_box: lib.az_box.clone(),
        library_empty: lib.outer.clone().upcast(),
        browse_stack: lib.browse_stack.clone(),
        artists_box: lib.artists_box.clone(),
        folders_box: lib.folders_box.clone(),
        tracks_box: lib.tracks_box.clone(),
        search_entry: search_entry.clone(),
        search_results,
        filter: RefCell::new(Filter::All),
        filter_buttons,
        queue_box,
    });

    wire(&state, &ui, &open_btn, &menu_btn, &mp_play, &np.play, &np.shuffle, &np.repeat, &np.prev, &np.next, &np.rewind, &np.fwd);
    wire_segmented(&state, &ui, &lib.seg);

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
    let toggles = gtk::Box::new(Orientation::Horizontal, 9);
    toggles.set_halign(gtk::Align::Center);
    toggles.append(&shuffle);
    toggles.append(&repeat);

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
            format,
            stack: stack.clone(),
        },
    )
}

/// Widgets the library page exposes to the rest of the app.
struct LibUi {
    nav: adw::NavigationView,
    albums_flow: gtk::FlowBox,
    az_box: gtk::Box,
    outer: gtk::Stack, // "empty" | "browse"
    browse_stack: gtk::Stack,
    artists_box: gtk::Box,
    folders_box: gtk::Box,
    tracks_box: gtk::Box,
    seg: Vec<(&'static str, gtk::ToggleButton)>,
}

/// The four browse tabs, in display order.
const BROWSE_TABS: [(&str, &str); 4] = [
    ("albums", "Albums"),
    ("artists", "Artists"),
    ("folders", "Folders"),
    ("tracks", "Tracks"),
];

fn build_library() -> LibUi {
    // --- Albums grid (with A–Z fast index) ---
    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(SelectionMode::None);
    // Fixed 3-up grid, matching the design's `repeat(3, …)`. FlowBox squeezes the
    // homogeneous cells to a third of the width rather than dropping to 2 columns.
    flow.set_min_children_per_line(3);
    flow.set_max_children_per_line(3);
    flow.set_homogeneous(true);
    flow.set_column_spacing(12);
    flow.set_row_spacing(12);
    flow.set_valign(gtk::Align::Start);

    let scroller = wrap_scroller(&flow);
    scroller.set_hexpand(true);

    let az = gtk::Box::new(Orientation::Vertical, 1);
    az.add_css_class("az-index");
    az.set_valign(gtk::Align::Center);
    az.set_margin_end(2);

    let albums_row = gtk::Box::new(Orientation::Horizontal, 0);
    albums_row.append(&scroller);
    albums_row.append(&az);

    // --- Artists / Folders / Tracks list tabs ---
    let artists_box = list_tab_box();
    let folders_box = list_tab_box();
    let tracks_box = list_tab_box();

    let browse_stack = gtk::Stack::new();
    browse_stack.set_vexpand(true);
    browse_stack.add_named(&albums_row, Some("albums"));
    browse_stack.add_named(&wrap_scroller(&artists_box), Some("artists"));
    browse_stack.add_named(&wrap_scroller(&folders_box), Some("folders"));
    browse_stack.add_named(&wrap_scroller(&tracks_box), Some("tracks"));
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
    let header = gtk::Box::new(Orientation::Horizontal, 12);
    header.set_margin_top(12);
    header.set_margin_start(16);
    header.set_margin_end(16);
    header.set_margin_bottom(8);
    header.append(&segbar);

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
        albums_flow: flow,
        az_box: az,
        outer,
        browse_stack,
        artists_box,
        folders_box,
        tracks_box,
        seg,
    }
}

/// A vertically-stacked, padded container for a browse-tab list.
fn list_tab_box() -> gtk::Box {
    let b = gtk::Box::new(Orientation::Vertical, 0);
    b.set_margin_top(8);
    b.set_margin_start(16);
    b.set_margin_end(16);
    b.set_margin_bottom(20);
    b
}

fn build_search() -> (gtk::Widget, gtk::SearchEntry, gtk::Box, Vec<(Filter, gtk::ToggleButton)>) {
    let entry = gtk::SearchEntry::new();
    entry.set_placeholder_text(Some("Search your library"));
    entry.set_hexpand(true);

    let chips_inner = gtk::Box::new(Orientation::Horizontal, 8);
    let filters = [
        ("All", Filter::All),
        ("Albums", Filter::Albums),
        ("Artists", Filter::Artists),
        ("Album Artists", Filter::AlbumArtists),
        ("Composers", Filter::Composers),
        ("Genres", Filter::Genres),
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

    // search: typed query + filter chips
    {
        let (state, ui) = (state.clone(), ui.clone());
        let entry = ui.search_entry.clone();
        entry.connect_search_changed(move |_| run_search(&state, &ui));
    }
    for (f, btn) in &ui.filter_buttons {
        let (state, ui, f) = (state.clone(), ui.clone(), *f);
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
            run_search(&state, &ui);
        });
    }

    // album activated → open its detail page
    {
        let (state, ui) = (state.clone(), ui.clone());
        let flow = ui.albums_flow.clone();
        flow.connect_child_activated(move |_, child| {
            let i = child.index() as usize;
            open_album_detail(&state, &ui, i);
        });
    }
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
        s.player.play(track.path.clone());
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
    let (album, tracks, art_dir) = {
        let s = state.borrow();
        let Some(al) = s.albums.get(index).cloned() else { return };
        let tracks = s
            .library
            .album_tracks(&al.album, al.album_artist.as_deref())
            .unwrap_or_default();
        (al, tracks, s.art_dir.clone())
    };
    let page = build_album_detail(state, ui, &album, tracks, &art_dir);
    ui.nav.push(&page);
}

/// Build an album-detail navigation page: a hero header (art + titles), the
/// Play / Shuffle / Add-to-queue actions, and the track list.
fn build_album_detail(
    state: &SharedState,
    ui: &SharedUi,
    album: &Album,
    tracks: Vec<Track>,
    art_dir: &Path,
) -> adw::NavigationPage {
    let tracks = Rc::new(tracks);

    let header = gtk::Box::new(Orientation::Horizontal, 16);
    let art = art_widget(art_dir, album.art_hash.as_deref(), 132, true);
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
        let n = tr.track_no.map(|n| format!("{n}.  ")).unwrap_or_default();
        let row = row_widget(
            art_dir,
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

    let content = gtk::Box::new(Orientation::Vertical, 16);
    content.set_margin_top(16);
    content.set_margin_bottom(20);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(&header);
    content.append(&actions);
    content.append(&lb);

    adw::NavigationPage::new(&wrap_scroller(&content), &album.album)
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
    let (queue, current, art_dir) = {
        let s = state.borrow();
        (s.queue.clone(), s.current, s.art_dir.clone())
    };
    if queue.is_empty() {
        let l = gtk::Label::new(Some("Queue is empty — play an album or add tracks from search."));
        l.add_css_class("dim-label");
        l.set_wrap(true);
        l.set_margin_top(16);
        ui.queue_box.append(&l);
        return;
    }
    let lb = boxed_list();
    for (i, t) in queue.iter().enumerate() {
        let (s1, u1) = (state.clone(), ui.clone());
        let (s2, u2) = (state.clone(), ui.clone());
        let subtitle = if current == Some(i) {
            format!("▶ {}", t.subtitle())
        } else {
            t.subtitle()
        };
        let row = row_widget(
            &art_dir,
            t.art_hash.as_deref(),
            &t.display_title(),
            &subtitle,
            None,
            Some(("list-remove-symbolic", "Remove from queue")),
            move || jump_to(&s1, &u1, i),
            move || remove_from_queue(&s2, &u2, i),
        );
        lb.append(&row);
    }
    ui.queue_box.append(&lb);
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
    let albums = state.borrow().library.albums(player_library::Sort::Title).unwrap_or_default();
    let art_dir = state.borrow().art_dir.clone();
    let n_tracks = state
        .borrow()
        .library
        .stats()
        .map(|s| s.tracks)
        .unwrap_or(0);

    while let Some(c) = ui.albums_flow.first_child() {
        ui.albums_flow.remove(&c);
    }
    while let Some(c) = ui.az_box.first_child() {
        ui.az_box.remove(&c);
    }

    if n_tracks == 0 {
        if let Some(s) = ui.library_empty.downcast_ref::<gtk::Stack>() {
            s.set_visible_child_name("empty");
        }
        state.borrow_mut().queue_albums(albums);
        return;
    }
    if let Some(s) = ui.library_empty.downcast_ref::<gtk::Stack>() {
        s.set_visible_child_name("browse");
    }

    for al in &albums {
        ui.albums_flow.append(&album_cell(&art_dir, al));
    }
    build_az_index(state, ui, &albums);

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

/// Populate and reveal a browse tab. Albums are already filled by
/// [`refresh_library`]; Artists/Folders/Tracks are filled on demand here.
fn show_browse_tab(state: &SharedState, ui: &SharedUi, name: &str) {
    ui.browse_stack.set_visible_child_name(name);
    let art_dir = state.borrow().art_dir.clone();
    match name {
        "artists" => {
            clear_box(&ui.artists_box);
            let artists = state.borrow().library.artists().unwrap_or_default();
            let lb = boxed_list();
            for a in &artists {
                let (state, ui, name) = (state.clone(), ui.clone(), a.name.clone());
                let meta = format!(
                    "{} album{} · {} track{}",
                    a.album_count,
                    if a.album_count == 1 { "" } else { "s" },
                    a.track_count,
                    if a.track_count == 1 { "" } else { "s" }
                );
                let row = row_widget(
                    &art_dir,
                    None,
                    &a.name,
                    &meta,
                    None,
                    Some(("media-playback-start-symbolic", "Play all")),
                    {
                        let (state, ui, name) = (state.clone(), ui.clone(), name.clone());
                        move || play_artist(&state, &ui, &name)
                    },
                    move || play_artist(&state, &ui, &name),
                );
                lb.append(&row);
            }
            ui.artists_box.append(&lb);
        }
        "folders" => {
            clear_box(&ui.folders_box);
            let folders = state.borrow().library.folders().unwrap_or_default();
            let lb = boxed_list();
            for f in &folders {
                let (state, ui, path) = (state.clone(), ui.clone(), f.path.clone());
                let row = row_widget(
                    &art_dir,
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
            ui.folders_box.append(&lb);
        }
        "tracks" => {
            clear_box(&ui.tracks_box);
            let tracks = state
                .borrow()
                .library
                .tracks(player_library::Sort::Artist)
                .unwrap_or_default();
            let lb = boxed_list();
            let all = Rc::new(tracks.clone());
            for (i, t) in tracks.iter().enumerate() {
                let (state, ui, all) = (state.clone(), ui.clone(), all.clone());
                let (s2, u2, tc) = (state.clone(), ui.clone(), t.clone());
                let row = row_widget(
                    &art_dir,
                    t.art_hash.as_deref(),
                    &t.display_title(),
                    &t.subtitle(),
                    Some(&t.format_spec()),
                    Some(("list-add-symbolic", "Add to queue")),
                    move || play_list(&state, &ui, (*all).clone(), i),
                    move || enqueue_track(&s2, &u2, tc.clone()),
                );
                lb.append(&row);
            }
            ui.tracks_box.append(&lb);
        }
        _ => {}
    }
}

fn clear_box(b: &gtk::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
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

fn build_az_index(state: &SharedState, ui: &SharedUi, albums: &[Album]) {
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
        let (ui2, idx) = (ui.clone(), i as i32);
        b.connect_clicked(move |_| {
            if let Some(child) = ui2.albums_flow.child_at_index(idx) {
                child.grab_focus();
            }
        });
        ui.az_box.append(&b);
    }
    let _ = state;
}

fn run_search(state: &SharedState, ui: &SharedUi) {
    let query = ui.search_entry.text().to_string();
    let filter = *ui.filter.borrow();
    let art_dir = state.borrow().art_dir.clone();

    // clear
    while let Some(c) = ui.search_results.first_child() {
        ui.search_results.remove(&c);
    }
    if query.trim().is_empty() {
        return;
    }

    let grouped = state
        .borrow()
        .library
        .search_grouped(&query, filter)
        .unwrap_or_default();
    // Tracks: nucleo typeahead for the forgiving "All" case, FTS otherwise.
    let tracks = if filter == Filter::All {
        state.borrow().library.search_typeahead(&query, 50)
    } else {
        grouped.tracks.clone()
    };

    if !grouped.albums.is_empty() {
        ui.search_results.append(&section_label("Albums"));
        let lb = boxed_list();
        for al in &grouped.albums {
            let title = al.album.clone();
            let artist = al.album_artist.clone();
            let meta = al.meta();
            let art = al.art_hash.clone();
            let (state, ui, alc) = (state.clone(), ui.clone(), al.clone());
            let row = row_widget(
                &art_dir,
                art.as_deref(),
                &title,
                artist.as_deref().unwrap_or("Unknown Artist"),
                Some(&meta),
                Some(("media-playback-start-symbolic", "Play album")),
                {
                    // Activate → album detail (drill-down).
                    let (state, ui, alc) = (state.clone(), ui.clone(), alc.clone());
                    move || {
                        let (tracks, art_dir) = {
                            let s = state.borrow();
                            (
                                s.library
                                    .album_tracks(&alc.album, alc.album_artist.as_deref())
                                    .unwrap_or_default(),
                                s.art_dir.clone(),
                            )
                        };
                        let page = build_album_detail(&state, &ui, &alc, tracks, &art_dir);
                        ui.nav.push(&page);
                        ui.stack.set_visible_child_name("library");
                    }
                },
                {
                    // Trailing ▶ → play the album immediately.
                    let (state, ui, alc) = (state.clone(), ui.clone(), alc.clone());
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

    if !grouped.folders.is_empty() {
        ui.search_results.append(&section_label("Folders"));
        let lb = boxed_list();
        for f in &grouped.folders {
            let row = row_widget(
                &art_dir,
                None,
                f.name(),
                &f.path,
                Some(&f.meta()),
                None,
                || {},
                || {},
            );
            lb.append(&row);
        }
        ui.search_results.append(&lb);
    }

    ui.search_results.append(&section_label("Tracks"));
    let lb = boxed_list();
    let tracks_rc = Rc::new(tracks.clone());
    for (i, t) in tracks.iter().enumerate() {
        let (state, ui, all) = (state.clone(), ui.clone(), tracks_rc.clone());
        let (s2, u2, tclone) = (state.clone(), ui.clone(), t.clone());
        let row = row_widget(
            &art_dir,
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

// ---------------------------------------------------------------------------
// Now-playing display helpers
// ---------------------------------------------------------------------------

fn show_track(state: &SharedState, ui: &SharedUi, track: &Track) {
    let art_dir = state.borrow().art_dir.clone();
    ui.title.set_subtitle(&format!("▶ {}", track.display_title()));
    ui.np_title.set_label(&track.display_title());
    ui.np_subtitle.set_label(&track.subtitle());
    ui.np_total
        .set_label(&fmt_dur_ms(track.duration_ms.unwrap_or(0)));
    ui.np_elapsed.set_label("0:00");
    ui.np_seek.set_value(0.0);
    ui.mp_progress.set_fraction(0.0);
    fill(&ui.np_art, &art_widget(&art_dir, track.art_hash.as_deref(), ART_HERO, true));
    fill(&ui.mp_art, &art_widget(&art_dir, track.art_hash.as_deref(), ART_MINI, false));
    ui.mp_title.set_label(&track.display_title());
    ui.mp_artist.set_label(&track.subtitle());
    fill(&ui.np_format, &format_chip(&track.signal_spec()));
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
        Ok(mut lib) => {
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
                    let _ = state.borrow_mut().library.refresh();
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

/// Open the preferences window: output device, library folder + live watch, theme.
fn open_settings(state: &SharedState, ui: &SharedUi) {
    let win = adw::PreferencesWindow::new();
    win.set_title(Some("Settings"));
    win.set_transient_for(Some(&ui.window));
    win.set_modal(true);
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
        let win = win.clone();
        choose.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Choose your music folder").build();
            let (state, ui, folder_row) = (state.clone(), ui.clone(), folder_row.clone());
            dialog.select_folder(Some(&win), gio::Cancellable::NONE, move |res| {
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

    win.add(&page);
    win.present();
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

fn add_page(stack: &adw::ViewStack, child: &impl IsA<gtk::Widget>, name: &str, title: &str, icon: &str) {
    let page = stack.add_titled(child, Some(name), title);
    page.set_icon_name(Some(icon));
}

fn circle(icon: &str, size: i32, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("circular");
    b.add_css_class("flat");
    b.set_size_request(size, size);
    b.set_tooltip_text(Some(tip));
    b
}

fn flat_menu_item(icon: &str, label: &str) -> gtk::Button {
    let b = gtk::Button::new();
    b.add_css_class("flat");
    let row = gtk::Box::new(Orientation::Horizontal, 10);
    let i = gtk::Image::from_icon_name(icon);
    let l = gtk::Label::new(Some(label));
    l.set_xalign(0.0);
    l.set_hexpand(true);
    row.append(&i);
    row.append(&l);
    b.set_child(Some(&row));
    b
}

fn section_label(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(&text.to_uppercase()));
    l.add_css_class("section-label");
    l.set_xalign(0.0);
    l
}

fn boxed_list() -> gtk::ListBox {
    let lb = gtk::ListBox::new();
    lb.add_css_class("boxed-list");
    lb.set_selection_mode(SelectionMode::None);
    lb.set_margin_bottom(16);
    lb
}

/// A boxed-list row: art · title/subtitle · mono meta · optional trailing button.
fn row_widget(
    art_dir: &Path,
    art_hash: Option<&str>,
    title: &str,
    subtitle: &str,
    meta: Option<&str>,
    trailing: Option<(&str, &str)>,
    on_activate: impl Fn() + 'static,
    on_trailing: impl Fn() + 'static,
) -> gtk::ListBoxRow {
    let row = gtk::Box::new(Orientation::Horizontal, 13);
    row.set_margin_top(8);
    row.set_margin_bottom(8);
    row.set_margin_start(12);
    row.set_margin_end(8);
    row.append(&art_widget(art_dir, art_hash, ART_ROW, false));

    let texts = gtk::Box::new(Orientation::Vertical, 1);
    texts.set_hexpand(true);
    texts.set_valign(gtk::Align::Center);
    let t = gtk::Label::new(Some(title));
    t.add_css_class("heading");
    t.set_xalign(0.0);
    t.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let s = gtk::Label::new(Some(subtitle));
    s.add_css_class("dim-label");
    s.set_xalign(0.0);
    s.set_ellipsize(gtk::pango::EllipsizeMode::End);
    texts.append(&t);
    texts.append(&s);
    row.append(&texts);

    if let Some(m) = meta {
        let ml = gtk::Label::new(Some(m));
        ml.add_css_class("dim-label");
        ml.add_css_class("mono");
        ml.add_css_class("caption");
        row.append(&ml);
    }

    if let Some((icon, tip)) = trailing {
        let b = circle(icon, 32, tip);
        b.connect_clicked(move |_| on_trailing());
        row.append(&b);
    }

    let lbr = gtk::ListBoxRow::new();
    lbr.set_child(Some(&row));
    lbr.set_activatable(true);
    // `ListBoxRow::activate` is a *keyboard* action signal — it does NOT fire on a
    // pointer tap (that was the "touching a track does nothing" bug). Drive taps
    // with an explicit `GestureClick` and keep `connect_activate` for the Enter
    // key. The trailing circle button claims its own clicks, so tapping it won't
    // also fire the row gesture.
    let on_activate = Rc::new(on_activate);
    lbr.connect_activate({
        let on_activate = on_activate.clone();
        move |_| on_activate()
    });
    let tap = gtk::GestureClick::new();
    tap.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        on_activate();
    });
    lbr.add_controller(tap);
    lbr
}

fn album_cell(art_dir: &Path, al: &Album) -> gtk::Box {
    let cell = gtk::Box::new(Orientation::Vertical, 6);
    let art = art_widget(art_dir, al.art_hash.as_deref(), 110, false);
    art.set_size_request(110, 110);
    cell.append(&art);
    let t = gtk::Label::new(Some(&al.album));
    t.add_css_class("heading");
    t.set_xalign(0.0);
    t.set_ellipsize(gtk::pango::EllipsizeMode::End);
    t.set_max_width_chars(14);
    let a = gtk::Label::new(Some(al.album_artist.as_deref().unwrap_or("Unknown Artist")));
    a.add_css_class("dim-label");
    a.add_css_class("caption");
    a.set_xalign(0.0);
    a.set_ellipsize(gtk::pango::EllipsizeMode::End);
    a.set_max_width_chars(14);
    cell.append(&t);
    cell.append(&a);
    cell
}

fn art_widget(art_dir: &Path, hash: Option<&str>, size: i32, big: bool) -> gtk::Widget {
    // A fixed square `Box` with `overflow: hidden` + the `.art` radius reliably
    // clips its child to rounded corners (the standard GTK4 recipe — an
    // `AspectFrame` does not). The fixed equal size_request plus a Cover-fit
    // `Picture` makes the art a centred square crop regardless of source shape.
    let frame = gtk::Box::new(Orientation::Vertical, 0);
    frame.set_size_request(size, size);
    frame.set_halign(gtk::Align::Center);
    frame.set_valign(gtk::Align::Center);
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.add_css_class("art");
    if big {
        frame.add_css_class("art-lg");
    }
    match hash {
        Some(h) if art_dir.join(h).exists() => {
            let pic = gtk::Picture::for_filename(art_dir.join(h));
            pic.set_content_fit(ContentFit::Cover);
            pic.set_can_shrink(true);
            pic.set_hexpand(true);
            pic.set_vexpand(true);
            frame.append(&pic);
        }
        _ => {
            frame.add_css_class("art-placeholder");
            let icon = gtk::Image::from_icon_name("folder-music-symbolic");
            icon.set_pixel_size((size as f32 * 0.42) as i32);
            icon.add_css_class("dim-label");
            icon.set_hexpand(true);
            icon.set_vexpand(true);
            frame.append(&icon);
        }
    }
    frame.upcast()
}

fn format_chip(spec: &str) -> gtk::Box {
    let chip = gtk::Box::new(Orientation::Horizontal, 8);
    chip.add_css_class("format-chip");
    chip.set_halign(gtk::Align::Center);
    let check = gtk::Image::from_icon_name("emblem-ok-symbolic");
    check.set_pixel_size(15);
    let label = gtk::Label::new(Some("Bit-perfect"));
    let specl = gtk::Label::new(Some(spec));
    specl.add_css_class("spec");
    chip.append(&check);
    chip.append(&label);
    chip.append(&specl);
    chip
}

fn wrap_scroller(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let s = gtk::ScrolledWindow::new();
    s.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    s.set_vexpand(true);
    s.set_child(Some(child));
    s
}

fn fill(container: &gtk::Box, child: &impl IsA<gtk::Widget>) {
    while let Some(c) = container.first_child() {
        container.remove(&c);
    }
    container.append(child);
}

fn toggle_accent(b: &gtk::Button, on: bool) {
    if on {
        b.add_css_class("accent");
    } else {
        b.remove_css_class("accent");
    }
}

fn quick_track(path: &Path) -> Track {
    Track {
        id: -1,
        path: path.to_path_buf(),
        folder: path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        title: path.file_stem().map(|s| s.to_string_lossy().into_owned()),
        artist: None,
        album_artist: None,
        album: None,
        composer: None,
        genre: None,
        track_no: None,
        disc_no: None,
        year: None,
        duration_ms: None,
        codec: None,
        sample_rate: None,
        bits: None,
        channels: None,
        art_hash: None,
    }
}

fn mmss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
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
