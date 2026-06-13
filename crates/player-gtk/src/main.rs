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
//!
//! `main.rs` is the assembly point: it builds each component (see the `ui`
//! modules), stores their long-lived handles in [`Ui`], and wires the
//! cross-cutting header/transport actions. Per-page behaviour lives with its
//! component module; playback/queue logic in `playback`, background bridges in
//! `events`.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::{gio, glib, Orientation};
use player_core::{Event, Player};
use player_library::Library;

mod art;
mod events;
mod hw_keys;
mod list;
mod playback;
mod standby;
mod state;
mod ui;
mod widgets;

use art::ArtCache;
use events::{on_ended, on_position, on_started, start_scan};
use playback::{
    advance, clear_queue, current_total_ms, current_track, load_playlist, play_list, prev_track,
    quick_track, restore_session, save_playlist, save_session, seek_by, seek_to_fraction,
    toggle_loved, toggle_play,
};
use state::{SharedState, SharedUi, State, Ui};
use ui::library::{
    album_cover_px, build_library, open_album_for, open_artist_detail, rebuild_albums,
    refresh_library, window_width, wire_segmented, wire_sort,
};
use ui::mini::build_mini;
use ui::now_playing::{build_now_playing, update_mini, update_now_playing_empty};
use ui::queue::refresh_queue;
use ui::search::{build_search, kick_search, render_search_results, spawn_search_worker};
use ui::settings::open_settings;
use widgets::{add_page, flat_menu_item, mmss, section_label, toggle_accent, wrap_scroller};

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
pub(crate) fn apply_theme(theme: Option<&str>) {
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
        // Phone-portrait by default (matches the Poco F1 / Phosh ~360 px logical
        // width); fully resizable on desktop. The content is built to fit this
        // width, so Phosh maximising to the screen never overflows.
        .default_width(360)
        .default_height(720)
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

    // NB: do NOT add a height-keyed AdwBreakpoint to hide the switcher when the
    // on-screen keyboard opens. On Phosh the OSK resizes the window via
    // text-input-v3; a max-height breakpoint that toggles a bottom bar's `reveal`
    // changes the content height, which re-triggers phoc's resize, oscillating
    // around the threshold and making the keyboard flicker open/closed. The
    // switcher simply stays put above the OSK (minor "double chrome"), which is
    // far better than the flicker. The Search page is keyboard-safe by structure
    // instead (AdwToolbarView pins the entry; only the results scroller shrinks).

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
        np_love: np.love.clone(),
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
        loved_scroller: lib.loved_scroller.clone(),
        sort: Cell::new(player_library::Sort::Title),
        sort_label: lib.sort_label.clone(),
        cover_px: Cell::new(110),
        search_entry: search_entry.clone(),
        search_results,
        filter: RefCell::new(player_library::Filter::Albums),
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

    // Rescale the 3-up album covers as the window resizes / (un)maximises so the
    // grid always fits the width. Rebuilds only when the quantised cover size
    // actually changes (see album_cover_px).
    {
        let rescale = {
            let (state, ui) = (state.clone(), ui.clone());
            move || {
                let px = album_cover_px(window_width(&ui));
                if px != ui.cover_px.get() {
                    ui.cover_px.set(px);
                    rebuild_albums(&state, &ui);
                }
            }
        };
        let rescale = Rc::new(rescale);
        let r1 = rescale.clone();
        ui.window.connect_default_width_notify(move |_| r1());
        ui.window.connect_maximized_notify(move |_| rescale());
    }

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
    // now-playing → love / unlove the current track
    {
        let (state, ui) = (state.clone(), ui.clone());
        np.love.connect_clicked(move |b| {
            let Some(t) = current_track(&state) else { return };
            if t.id <= 0 {
                ui.toast("This track isn't in your library");
                return;
            }
            let now = !b.has_css_class("loved");
            if now {
                b.add_css_class("loved");
            } else {
                b.remove_css_class("loved");
            }
            toggle_loved(&state, &ui, t.id, now);
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

    // Standby battery saver (Poco F1): suspend after an idle timeout when no USB
    // DAC is connected. Off unless PLAYER_STANDBY_TIMEOUT_SECS is set, so the
    // desktop dev box is never affected.
    let standby = standby::Standby::from_env();

    // evdev volume-key transport: works with screen off / app unfocused.
    // vol-down → pause/resume, vol-up → next track. Also counts as activity.
    {
        let (hw_tx, hw_rx) = async_channel::unbounded::<hw_keys::HwKey>();
        std::thread::spawn(move || hw_keys::run(hw_tx));
        let (state, ui) = (state.clone(), ui.clone());
        let standby = standby.clone();
        glib::spawn_future_local(async move {
            while let Ok(key) = hw_rx.recv().await {
                if let Some(sb) = &standby {
                    sb.note_activity();
                }
                match key {
                    hw_keys::HwKey::VolumeDown => toggle_play(&state, &ui),
                    hw_keys::HwKey::VolumeUp => advance(&state, &ui, true),
                }
            }
        });
    }

    // Any window input (touch/key/motion) is activity; then start the
    // idle→suspend evaluator on the main loop.
    if let Some(sb) = standby {
        let legacy = gtk::EventControllerLegacy::new();
        {
            let sb = sb.clone();
            legacy.connect_event(move |_, _| {
                sb.note_activity();
                glib::Propagation::Proceed
            });
        }
        ui.window.add_controller(legacy);
        sb.start(state.clone());
    }

    refresh_library(&state, &ui);
    update_now_playing_empty(&ui, true);
    restore_session(&state, &ui);
    refresh_queue(&state, &ui);
    window.present();
}

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
