//! The preferences dialog (output device · buffer · library folder + live watch
//! · theme) and the engine re-spawn used when the output device or buffer changes.

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::gio;
use player_core::Player;

use crate::apply_theme;
use crate::events::{set_watch, start_scan};
use crate::state::{SharedState, SharedUi};

/// Replace the running engine with one bound to `device` and the current
/// `buffer_periods`, reusing the existing event channel so the event pump keeps
/// working. Persists the device choice. Used for both device and buffer changes.
pub(crate) fn respawn_player(state: &SharedState, device: String) {
    let (ev_tx, periods) = {
        let s = state.borrow();
        (s.ev_tx.clone(), s.buffer_periods)
    };
    let new_player = Player::spawn_with(
        device.clone(),
        player_core::DEFAULT_PERIOD,
        periods,
        move |ev| {
            let _ = ev_tx.send_blocking(ev);
        },
    );
    let mut s = state.borrow_mut();
    s.player = new_player; // dropping the old player joins its threads
    let _ = s.library.set_meta("device", &device);
    s.device = device;
    s.playing = false;
    s.paused = false;
}

/// Open the preferences dialog: output device, library folder + live watch, theme.
pub(crate) fn open_settings(state: &SharedState, ui: &SharedUi) {
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

    // --- Output buffer (ALSA depth) ---
    // Buffer = DEFAULT_PERIOD * periods. Larger = more robust against xruns; we
    // favour robustness (an xrun is a guarded PCM re-init = the full-scale-burst
    // hazard, see CLAUDE.md), so the presets bias upward and the one smaller
    // option is flagged. period stays DEFAULT_PERIOD; only the depth varies.
    let buf_periods: [i64; 4] = [4, 8, 16, 32];
    let buf_row = adw::ComboRow::new();
    buf_row.set_title("Output buffer");
    buf_row.set_subtitle("Larger = fewer dropouts (xruns); changing it restarts playback");
    let buf_model = gtk::StringList::new(&[
        "Low latency · 4096 frames (more dropout risk)",
        "Default · 8192 frames",
        "Large · 16384 frames",
        "Largest · 32768 frames",
    ]);
    buf_row.set_model(Some(&buf_model));
    let cur_periods = state.borrow().buffer_periods;
    let cur_idx = buf_periods.iter().position(|&p| p == cur_periods).unwrap_or(1) as u32;
    buf_row.set_selected(cur_idx);
    {
        let (state, ui) = (state.clone(), ui.clone());
        buf_row.connect_selected_notify(move |r| {
            let periods = buf_periods[r.selected() as usize];
            if periods != state.borrow().buffer_periods {
                {
                    let mut s = state.borrow_mut();
                    s.buffer_periods = periods;
                    let _ = s.library.set_meta("audio_periods", &periods.to_string());
                }
                let device = state.borrow().device.clone();
                respawn_player(&state, device);
                ui.toast(&format!(
                    "Output buffer → {} frames",
                    periods * player_core::DEFAULT_PERIOD
                ));
            }
        });
    }
    out.add(&buf_row);
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
