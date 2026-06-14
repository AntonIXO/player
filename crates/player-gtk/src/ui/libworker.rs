//! Off-main-thread library queries. Mirrors `ui::search`'s worker: a dedicated
//! thread owns its own `Library` connection and runs the browse/detail queries so
//! a tab switch, album open, or play-all never blocks the GTK main loop with a
//! synchronous `SELECT`.
//!
//! A job is a `Send` closure `FnOnce(&Library) -> LibPayload`; its result is
//! applied back on the main loop by a continuation kept on the [`Ui`]. Two job
//! kinds:
//! - **render** jobs (tab / detail navigation) are *latest-wins*: each bumps
//!   `lib_render_seq`, and a result is applied only if it is still the latest — so
//!   tapping Folders while Artists is still loading drops the Artists result.
//! - **action** jobs (play / shuffle / enqueue a collection) are never dropped:
//!   their result always runs once the tracks are fetched.
//!
//! The worker only ever *reads*, so it adds no WAL write contention (the single
//! writer remains the main-thread connection: `record_play`/`set_loved`/`set_meta`).

use std::time::Duration;

use gtk4 as gtk;

use gtk::glib;
use gtk::prelude::*;
use player_library::{Album, Artist, Folder, Library, Track};

use crate::state::{SharedState, SharedUi};

/// A unit of work for the library worker thread.
pub(crate) struct LibJob {
    pub(crate) seq: u64,
    pub(crate) run: Box<dyn FnOnce(&Library) -> LibPayload + Send>,
}

/// A finished job's result, sent back to the main loop.
pub(crate) struct LibDone {
    pub(crate) seq: u64,
    pub(crate) payload: LibPayload,
}

/// The result shapes a job can produce. Every variant is `Send` (it only carries
/// owned model rows), so it crosses the worker→main channel freely.
pub(crate) enum LibPayload {
    Albums { albums: Vec<Album>, n_tracks: u64 },
    Artists(Vec<Artist>),
    Folders(Vec<Folder>),
    Tracks(Vec<Track>),
    Album { album: Album, tracks: Vec<Track> },
    ArtistAlbums(Vec<Album>),
}

/// A main-thread continuation applied to a finished job's payload. It receives
/// `state`/`ui` as parameters (rather than capturing them) so a pending entry
/// never forms an `Rc` cycle through the [`Ui`].
pub(crate) type Cont = Box<dyn FnOnce(&SharedState, &SharedUi, LibPayload)>;

/// Pending continuations keyed by job seq; the bool marks a render job (subject to
/// latest-wins) vs an action job (always applied).
pub(crate) type Pending = std::collections::HashMap<u64, (bool, Cont)>;

/// Spawn the worker. It owns its own `Library` connection and runs every job in
/// order — superseded renders are dropped on the main side, so there is no unsafe
/// coalescing that could swallow an action job.
pub(crate) fn spawn_library_worker(
    db: std::path::PathBuf,
    art: std::path::PathBuf,
) -> (async_channel::Sender<LibJob>, async_channel::Receiver<LibDone>) {
    let (jtx, jrx) = async_channel::unbounded::<LibJob>();
    let (rtx, rrx) = async_channel::unbounded::<LibDone>();
    std::thread::spawn(move || {
        let Ok(lib) = Library::open(&db, &art) else { return };
        while let Ok(job) = jrx.recv_blocking() {
            let payload = (job.run)(&lib);
            let _ = rtx.send_blocking(LibDone { seq: job.seq, payload });
        }
    });
    (jtx, rrx)
}

/// The worker→main result pump: apply each finished job's continuation, honouring
/// latest-wins for render jobs. Spawned once from `main`.
pub(crate) fn spawn_pump(
    state: SharedState,
    ui: SharedUi,
    rx: async_channel::Receiver<LibDone>,
) {
    glib::spawn_future_local(async move {
        while let Ok(done) = rx.recv().await {
            let entry = ui.lib_pending.borrow_mut().remove(&done.seq);
            if let Some((is_render, cont)) = entry {
                if is_render && done.seq != ui.lib_render_seq.get() {
                    continue; // a newer render superseded this one
                }
                cont(&state, &ui, done.payload);
            }
        }
    });
}

/// Queue a job; returns its seq. `render` jobs participate in latest-wins; `action`
/// jobs always run.
fn submit(
    ui: &SharedUi,
    render: bool,
    run: impl FnOnce(&Library) -> LibPayload + Send + 'static,
    then: impl FnOnce(&SharedState, &SharedUi, LibPayload) + 'static,
) -> u64 {
    let seq = ui.lib_seq.get().wrapping_add(1);
    ui.lib_seq.set(seq);
    if render {
        ui.lib_render_seq.set(seq);
    }
    ui.lib_pending.borrow_mut().insert(seq, (render, Box::new(then)));
    let _ = ui.lib_tx.try_send(LibJob { seq, run: Box::new(run) });
    seq
}

/// Submit a navigation/render query (latest-wins). Returns the job seq so the
/// caller can arm a [`spinner_after`] on the target scroller.
pub(crate) fn submit_render(
    ui: &SharedUi,
    run: impl FnOnce(&Library) -> LibPayload + Send + 'static,
    then: impl FnOnce(&SharedState, &SharedUi, LibPayload) + 'static,
) -> u64 {
    submit(ui, true, run, then)
}

/// Submit a play/shuffle/enqueue query whose result must always run.
pub(crate) fn submit_action(
    ui: &SharedUi,
    run: impl FnOnce(&Library) -> LibPayload + Send + 'static,
    then: impl FnOnce(&SharedState, &SharedUi, LibPayload) + 'static,
) {
    submit(ui, false, run, then);
}

/// Show a centred spinner in `scroller` after ~120 ms *if* the render job `seq` is
/// still the latest pending one and the scroller has no content yet — so a fast
/// query never flashes a spinner, and a revisited tab keeps its previous list
/// until the fresh one arrives, while a slow first load shows progress.
pub(crate) fn spinner_after(ui: &SharedUi, scroller: &gtk::ScrolledWindow, seq: u64) {
    let (ui, scroller) = (ui.clone(), scroller.clone());
    glib::timeout_add_local_once(Duration::from_millis(120), move || {
        let still_pending =
            ui.lib_render_seq.get() == seq && ui.lib_pending.borrow().contains_key(&seq);
        if still_pending && scroller.child().is_none() {
            let sp = gtk::Spinner::new();
            sp.set_spinning(true);
            sp.set_size_request(32, 32);
            sp.set_halign(gtk::Align::Center);
            sp.set_valign(gtk::Align::Center);
            sp.set_margin_top(24);
            scroller.set_child(Some(&sp));
        }
    });
}
