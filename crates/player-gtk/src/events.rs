//! Bridges from background work to the GTK main loop: the engine event
//! handlers (Started/Position/Ended), the library-scan worker, and the live
//! folder-watch bridge. All UI mutation happens here on the main thread.

use std::path::PathBuf;
use std::time::Duration;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::glib;
use player_core::StreamSpec;
use player_library::{fmt_khz, Library};

use crate::playback::advance;
use crate::state::{SharedState, SharedUi};
use crate::ui::library::refresh_library;
use crate::ui::now_playing::set_play_icon;
use crate::ui::search::SearchMsg;
use crate::widgets::{fill, format_chip, mmss};

// ---------------------------------------------------------------------------
// Engine event handlers
// ---------------------------------------------------------------------------

pub(crate) fn on_started(state: &SharedState, ui: &SharedUi, spec: StreamSpec) {
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
    let chip = if spec.is_dop {
        // DoP: a PCM wire carrying DSD. The PCM rate is the DSD rate / 16; recover
        // the DSD multiple (176.4 kHz → DSD64) for a meaningful label.
        format!(
            "DSD{} · DoP · {} · {}",
            spec.rate * 16 / 44_100,
            fmt_khz(spec.rate),
            spec.fmt.label()
        )
    } else {
        format!(
            "{}-bit · {} · {}",
            spec.source_bits,
            fmt_khz(spec.rate),
            spec.fmt.label()
        )
    };
    fill(&ui.np_format, &format_chip(&chip));
}

pub(crate) fn on_position(state: &SharedState, ui: &SharedUi, frames: u64) {
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

pub(crate) fn on_ended(state: &SharedState, ui: &SharedUi) {
    advance(state, ui, false);
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
pub(crate) fn start_scan(state: &SharedState, ui: &SharedUi, root: PathBuf, force: bool) {
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

/// Enable or disable live folder-watching. When on, a background thread bridges
/// the `notify` events to the main loop, which debounces and re-scans.
pub(crate) fn set_watch(state: &SharedState, ui: &SharedUi, on: bool) {
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
