//! Headless PGO/self-test bench: when `PLAYER_GTK_BENCH` is set, drive the UI
//! through its own hot paths (page switches, search, album-grid + hero rescale,
//! detail pages) and quit — so a `-Cprofile-generate` build run under a headless
//! compositor collects coverage for player-gtk's OWN code (the shared decode/
//! library code is already covered by the player-cli workload). No-op otherwise.

use std::time::Duration;

use libadwaita as adw;

use adw::prelude::*;
use gtk4::glib;

use crate::state::{SharedState, SharedUi};
use crate::ui::library::{
    album_cover_px, open_album_for, open_artist_detail, rebuild_albums, refresh_library,
};
use crate::ui::now_playing::resize_hero;
use crate::ui::queue::refresh_queue;
use crate::ui::search::kick_search;

/// Drive the UI through its hot code paths on the main loop, then quit. Stepped
/// (one action per tick) so each layout/search/render settles; a watchdog
/// force-quits after `PLAYER_GTK_BENCH_SECS` (default 30) so the build can never
/// hang on a stalled step. Only reached when `PLAYER_GTK_BENCH` is set.
pub(crate) fn run_bench(app: &adw::Application, state: &SharedState, ui: &SharedUi) {
    let secs: u64 = std::env::var("PLAYER_GTK_BENCH_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    {
        let app = app.clone();
        glib::timeout_add_local_once(Duration::from_secs(secs), move || app.quit());
    }

    let app = app.clone();
    let (state, ui) = (state.clone(), ui.clone());
    let mut step = 0u32;
    glib::timeout_add_local(Duration::from_millis(140), move || {
        match step {
            0 => refresh_library(&state, &ui),
            1 => ui.stack.set_visible_child_name("playing"),
            2 => {
                ui.stack.set_visible_child_name("search");
                ui.search_entry.set_text("love");
                kick_search(&ui);
            }
            3 => {
                ui.search_entry.set_text("концерт");
                kick_search(&ui);
            }
            4 => {
                ui.stack.set_visible_child_name("lists");
                refresh_queue(&state, &ui);
            }
            5 => {
                ui.stack.set_visible_child_name("library");
                // Width-derived album-grid + Now-Playing hero rescale.
                for w in [360i32, 540, 720] {
                    ui.cover_px.set(album_cover_px(w));
                    rebuild_albums(&state, &ui);
                    resize_hero(&state, &ui, w);
                }
            }
            6 => {
                // Album/artist detail pages, if the library populated by now.
                let first = state
                    .borrow()
                    .albums
                    .first()
                    .map(|a| (a.album.clone(), a.album_artist.clone()));
                if let Some((album, aa)) = first {
                    open_album_for(&state, &ui, &album, aa.as_deref());
                    if let Some(a) = aa.filter(|a| !a.is_empty()) {
                        open_artist_detail(&state, &ui, &a);
                    }
                }
            }
            7 => ui.stack.set_visible_child_name("library"),
            // Settle: let the async search/library-worker renders and layout
            // passes actually run (so their code is covered) before quitting.
            8..=24 => {}
            _ => {
                app.quit();
                return glib::ControlFlow::Break;
            }
        }
        step += 1;
        glib::ControlFlow::Continue
    });
}
