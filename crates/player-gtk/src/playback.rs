//! Playback control and the queue. The app drives the queue track-by-track
//! (advancing on the engine's `Ended` event); the engine itself only knows
//! play/enqueue/seek/stop. Also holds session + `.m3u` persistence.

use std::path::{Path, PathBuf};
use std::time::Duration;

use libadwaita as adw;

use adw::prelude::*;
use player_library::Track;

use crate::state::{SharedState, SharedUi};
use crate::ui::libworker::{submit_action, LibPayload};
use crate::ui::now_playing::{set_play_icon, show_track, update_mini, update_now_playing_empty};
use crate::ui::queue::refresh_queue;
use crate::widgets::mmss;

pub(crate) fn play_list(state: &SharedState, ui: &SharedUi, tracks: Vec<Track>, start: usize) {
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

pub(crate) fn start_current(state: &SharedState, ui: &SharedUi) {
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
        // SACD .iso track → the DoP path; a .cue track decodes a sub-range of its
        // source file; everything else is a whole-file play.
        if let Some(t) = track.sacd_track() {
            s.player.play_sacd(track.source_path.clone().unwrap_or_else(|| track.path.clone()), t);
        } else {
            match track.cue_range() {
                Some((start, end)) => s.player.play_range(track.source_path.clone().unwrap_or_else(|| track.path.clone()), start, end),
                None => s.player.play(track.path.clone()),
            }
        }
        // Log to recently-played history (indexed tracks only).
        if track.id > 0 {
            let _ = s.library.record_play(track.id);
        }
    }
    update_mini(state, ui);
    refresh_queue(state, ui);
}

pub(crate) fn toggle_play(state: &SharedState, ui: &SharedUi) {
    let (playing, paused, has_current, has_queue) = {
        let s = state.borrow();
        (s.playing, s.paused, s.current.is_some(), !s.queue.is_empty())
    };
    if playing {
        pause_playback(state, ui);
    } else if paused {
        resume_playback(state, ui);
    } else if has_current {
        state.borrow_mut().playing = true;
        start_current(state, ui);
    } else if has_queue {
        {
            let mut s = state.borrow_mut();
            s.current = Some(0);
            s.playing = true;
        }
        start_current(state, ui);
    }
}

/// Real pause: hold output at the device, keep the track loaded.
fn pause_playback(state: &SharedState, ui: &SharedUi) {
    state.borrow().player.pause();
    {
        let mut s = state.borrow_mut();
        s.playing = false;
        s.paused = true;
    }
    set_play_icon(ui, false);
}

/// Resume the held track (do not restart it).
fn resume_playback(state: &SharedState, ui: &SharedUi) {
    state.borrow().player.resume();
    {
        let mut s = state.borrow_mut();
        s.playing = true;
        s.paused = false;
    }
    set_play_icon(ui, true);
}

/// Total duration (ms) of the currently-selected queue track, or 0 if unknown.
pub(crate) fn current_total_ms(state: &SharedState) -> u64 {
    let s = state.borrow();
    s.current
        .and_then(|i| s.queue.get(i))
        .and_then(|t| t.duration_ms)
        .unwrap_or(0)
}

/// Seek the current track to `frac` (0..1) of its duration.
pub(crate) fn seek_to_fraction(state: &SharedState, ui: &SharedUi, frac: f64) {
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
pub(crate) fn seek_by(state: &SharedState, ui: &SharedUi, delta_ms: i64) {
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

pub(crate) fn advance(state: &SharedState, ui: &SharedUi, user: bool) {
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

pub(crate) fn prev_track(state: &SharedState, ui: &SharedUi) {
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

/// The track currently loaded in the queue, if any.
pub(crate) fn current_track(state: &SharedState) -> Option<Track> {
    let s = state.borrow();
    s.current.and_then(|i| s.queue.get(i).cloned())
}

pub(crate) fn enqueue_track(state: &SharedState, ui: &SharedUi, track: Track) {
    state.borrow_mut().queue.push(track);
    refresh_queue(state, ui);
    ui.toast("Added to queue");
}

/// Persist a track's loved state (the heart widget toggles its own look first).
/// No-op with a toast for ad-hoc, non-indexed tracks (`id <= 0`). Keeps in-memory
/// queue copies in sync and refreshes the Loved tab if it is the one on screen.
pub(crate) fn toggle_loved(state: &SharedState, ui: &SharedUi, track_id: i64, loved: bool) {
    if track_id <= 0 {
        ui.toast("This track isn't in your library");
        return;
    }
    if state.borrow().library.set_loved(track_id, loved).is_err() {
        ui.toast("Couldn't update loved tracks");
        return;
    }
    for t in state.borrow_mut().queue.iter_mut() {
        if t.id == track_id {
            t.loved = loved;
        }
    }
    if ui.browse_stack.visible_child_name().as_deref() == Some("loved") {
        crate::ui::library::show_browse_tab(state, ui, "loved");
    }
}

pub(crate) fn enqueue_tracks(state: &SharedState, ui: &SharedUi, tracks: Vec<Track>) {
    let n = tracks.len();
    state.borrow_mut().queue.extend(tracks);
    refresh_queue(state, ui);
    ui.toast(&format!("Added {n} track{} to queue", if n == 1 { "" } else { "s" }));
}

/// In-place time-seeded shuffle (no rng dependency; fine for playback order).
pub(crate) fn shuffle_vec<T>(v: &mut [T]) {
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

/// Append `track` to the queue and start playing it immediately.
pub(crate) fn play_track_now(state: &SharedState, ui: &SharedUi, track: Track) {
    let i = {
        let mut s = state.borrow_mut();
        s.queue.push(track);
        s.queue.len() - 1
    };
    jump_to(state, ui, i);
}

/// Stop playback and empty the queue.
pub(crate) fn clear_queue(state: &SharedState, ui: &SharedUi) {
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

pub(crate) fn jump_to(state: &SharedState, ui: &SharedUi, i: usize) {
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

pub(crate) fn remove_from_queue(state: &SharedState, ui: &SharedUi, i: usize) {
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

/// Play every track of `artist`. The query runs on the library worker (it can be
/// large), so playback starts as soon as the tracks are fetched without blocking
/// the UI. `_state` is unused — the worker continuation receives it from the pump.
pub(crate) fn play_artist(_state: &SharedState, ui: &SharedUi, artist: &str) {
    let name = artist.to_string();
    submit_action(
        ui,
        move |lib| LibPayload::Tracks(lib.artist_tracks(&name).unwrap_or_default()),
        |state, ui, p| {
            if let LibPayload::Tracks(v) = p {
                if v.is_empty() {
                    ui.toast("No tracks for that artist");
                    return;
                }
                play_list(state, ui, v, 0);
            }
        },
    );
}

/// Play every track directly inside `folder` (fetched on the library worker).
pub(crate) fn play_folder(_state: &SharedState, ui: &SharedUi, folder: &str) {
    let folder = folder.to_string();
    submit_action(
        ui,
        move |lib| LibPayload::Tracks(lib.folder_tracks(&folder).unwrap_or_default()),
        |state, ui, p| {
            if let LibPayload::Tracks(v) = p {
                if v.is_empty() {
                    ui.toast("No tracks in that folder");
                    return;
                }
                play_list(state, ui, v, 0);
            }
        },
    );
}

/// Play a whole album by `(album, album_artist)` (tracks fetched on the worker).
pub(crate) fn play_album(_state: &SharedState, ui: &SharedUi, album: &str, album_artist: Option<&str>) {
    let (a, aa) = (album.to_string(), album_artist.map(|s| s.to_string()));
    submit_action(
        ui,
        move |lib| LibPayload::Tracks(lib.album_tracks(&a, aa.as_deref()).unwrap_or_default()),
        |state, ui, p| {
            if let LibPayload::Tracks(v) = p {
                if v.is_empty() {
                    ui.toast("No tracks for this album");
                    return;
                }
                play_list(state, ui, v, 0);
            }
        },
    );
}

pub(crate) fn pseudo_random(len: usize) -> usize {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    n % len.max(1)
}

/// Build a [`Track`] for a file that may not be in the library (Open / playlist
/// / restored session). Delegates to the library extractor so the now-playing
/// view has a real title, duration, and sample rate — without which the seek bar
/// and elapsed clock would never move.
pub(crate) fn quick_track(path: &Path) -> Track {
    player_library::track_from_path(path)
}

// ---------------------------------------------------------------------------
// Session + playlist persistence
// ---------------------------------------------------------------------------

/// The queue's track paths as strings, in order — shared by the session save and
/// the `.m3u` export.
fn queue_paths(queue: &[Track]) -> Vec<String> {
    queue
        .iter()
        .map(|t| t.path.to_string_lossy().into_owned())
        .collect()
}

/// Persist the current queue, selected index, and position for next launch.
/// Written as one transaction (single WAL commit) so it does not stall the
/// closing window with a commit per key.
pub(crate) fn save_session(state: &SharedState) {
    let s = state.borrow();
    let queue = queue_paths(&s.queue).join("\n");
    let current = s.current.map(|i| i.to_string()).unwrap_or_default();
    let pos = s.last_pos_ms.to_string();
    let music_dir = s.music_dir.as_ref().map(|d| d.to_string_lossy().into_owned());
    let mut kv: Vec<(&str, &str)> = vec![
        ("queue", queue.as_str()),
        ("current", current.as_str()),
        ("pos_ms", pos.as_str()),
    ];
    if let Some(d) = &music_dir {
        kv.push(("music_dir", d.as_str()));
    }
    let _ = s.library.set_meta_many(&kv);
}

/// Restore the last session's queue/position. Loads the queue but does not
/// auto-play; pressing play resumes the current track at its saved position.
pub(crate) fn restore_session(state: &SharedState, ui: &SharedUi) {
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
        let paths: Vec<PathBuf> = queue_raw
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
        // One query resolves the whole queue; map the ordered paths through it,
        // falling back to a metadata-less stub for any path no longer indexed.
        let by_path = s.library.tracks_by_paths(&paths).unwrap_or_default();
        paths
            .iter()
            .map(|pb| by_path.get(pb).cloned().unwrap_or_else(|| quick_track(pb)))
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
pub(crate) fn save_playlist(state: &SharedState, ui: &SharedUi, path: PathBuf) {
    let paths = queue_paths(&state.borrow().queue);
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
pub(crate) fn load_playlist(state: &SharedState, ui: &SharedUi, path: PathBuf) {
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
