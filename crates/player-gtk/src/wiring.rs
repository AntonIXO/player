//! Cross-cutting action wiring: the header file/menu buttons, the transport
//! controls (play/pause/prev/next/seek/shuffle/repeat), the interactive seek
//! debounce, and the search entry/filters. Split out of `main.rs`; the assembly
//! point there calls [`wire`] once after the widgets and [`crate::state::Ui`] exist.

use std::time::Duration;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::{gio, glib, Orientation};

use crate::events::start_scan;
use crate::playback::{
    advance, current_total_ms, play_list, prev_track, quick_track, seek_by, seek_to_fraction,
    toggle_play,
};
use crate::state::{SharedState, SharedUi};
use crate::ui::now_playing::update_mini;
use crate::ui::search::kick_search;
use crate::ui::settings::open_settings;
use crate::widgets::{flat_menu_item, mmss, toggle_accent};

#[allow(clippy::too_many_arguments)]
pub(crate) fn wire(
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
