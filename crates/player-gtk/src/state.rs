//! Shared application state and the long-lived widget handles the closures
//! update. Both are wrapped in `Rc` and live for the whole session (single
//! GTK main-loop thread), so every component takes `&SharedState`/`&SharedUi`.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use libadwaita as adw;

use player_core::{Event, Player};
use player_library::{Album, Filter, Library, Sort, Track};

use crate::art::ArtCache;
use crate::ui::search::SearchMsg;

/// Mutable app state (single-threaded: GTK main loop).
pub(crate) struct State {
    pub(crate) library: Library,
    pub(crate) player: Player,
    pub(crate) db_path: PathBuf,
    pub(crate) art_dir: PathBuf,
    pub(crate) music_dir: Option<PathBuf>,
    pub(crate) queue: Vec<Track>,
    pub(crate) albums: Vec<Album>,
    pub(crate) current: Option<usize>,
    pub(crate) playing: bool,
    /// A track is loaded in the engine but held (pause, not stop) — resume
    /// continues it rather than restarting.
    pub(crate) paused: bool,
    pub(crate) repeat: bool,
    pub(crate) shuffle: bool,
    pub(crate) device: String,
    /// ALSA buffer depth (number of `DEFAULT_PERIOD`-frame periods) the engine
    /// opens with — user-tunable in Settings, persisted as the `audio_periods`
    /// meta key. Larger = more robust against xruns (we favour robustness; an
    /// xrun is a guarded PCM re-init = the burst hazard). Read on every re-spawn.
    pub(crate) buffer_periods: i64,
    /// Sender the engine's `emit` closure feeds; cloned when re-spawning the
    /// player (device change) so the existing event pump keeps working.
    pub(crate) ev_tx: async_channel::Sender<Event>,
    /// Latest elapsed position (ms) of the current track, for session save.
    pub(crate) last_pos_ms: u64,
    /// A position to seek to once the restored track first starts (resume).
    pub(crate) resume_to: Option<Duration>,
    /// Kept alive while live folder-watching is enabled.
    pub(crate) watcher: Option<player_library::LibraryWatcher>,
}

/// Long-lived widget handles the closures update.
pub(crate) struct Ui {
    pub(crate) window: adw::ApplicationWindow,
    pub(crate) toasts: adw::ToastOverlay,
    pub(crate) stack: adw::ViewStack,
    pub(crate) title: adw::WindowTitle,
    // mini player
    pub(crate) mini: gtk::Revealer,
    pub(crate) mp_art: gtk::Box,
    pub(crate) mp_title: gtk::Label,
    pub(crate) mp_artist: gtk::Label,
    pub(crate) mp_play: gtk::Button,
    pub(crate) mp_progress: gtk::ProgressBar,
    // now playing
    pub(crate) np_art: gtk::Box,
    pub(crate) np_title: gtk::Label,
    pub(crate) np_subtitle: gtk::Label,
    pub(crate) np_elapsed: gtk::Label,
    pub(crate) np_total: gtk::Label,
    pub(crate) np_seek: gtk::Scale,
    pub(crate) np_play: gtk::Button,
    pub(crate) np_love: gtk::Button,
    pub(crate) np_goto_artist: gtk::Button,
    pub(crate) np_goto_album: gtk::Button,
    pub(crate) np_format: gtk::Box,
    pub(crate) np_stack: gtk::Stack,
    /// True while the user is dragging the seek slider — suppresses programmatic
    /// position updates so the handle does not fight the drag.
    pub(crate) seeking: Cell<bool>,
    /// Monotonic seek-debounce generation: each slider move bumps it and arms a
    /// timeout; only the latest generation actually commits the engine seek.
    pub(crate) seek_gen: Cell<u64>,
    // library — each browse tab is a ScrolledWindow whose child is a freshly
    // built (virtualised) ListView/GridView, so only visible rows are realised.
    pub(crate) nav: adw::NavigationView,
    pub(crate) albums_scroller: gtk::ScrolledWindow,
    pub(crate) az_box: gtk::Box,
    pub(crate) library_empty: gtk::Widget, // outer Stack: "empty" | "browse"
    pub(crate) browse_stack: gtk::Stack,   // "albums" | "artists" | "folders" | "tracks"
    pub(crate) artists_scroller: gtk::ScrolledWindow,
    pub(crate) folders_scroller: gtk::ScrolledWindow,
    pub(crate) tracks_scroller: gtk::ScrolledWindow,
    pub(crate) loved_scroller: gtk::ScrolledWindow,
    /// Current browse sort (Albums/Tracks tabs); the menu button's label mirrors it.
    pub(crate) sort: Cell<Sort>,
    pub(crate) sort_label: gtk::Label,
    /// Album-cover edge length (px) for the 3-up grid, recomputed from the window
    /// width on resize so the covers scale (see `ui::library::album_cover_px`).
    pub(crate) cover_px: Cell<i32>,
    /// Now-Playing hero-art edge length (px), recomputed from the window width so
    /// the cover fills the (often wider-than-360) device screen without
    /// overflowing it (see `ui::now_playing::hero_art_px`).
    pub(crate) hero_px: Cell<i32>,
    // search
    pub(crate) search_entry: gtk::SearchEntry,
    pub(crate) search_results: gtk::Box,
    pub(crate) filter: RefCell<Filter>,
    pub(crate) filter_buttons: Vec<(Filter, gtk::ToggleButton)>,
    /// Sender into the background search worker (own connection + fuzzy index).
    pub(crate) search_tx: async_channel::Sender<SearchMsg>,
    /// Monotonic search generation: each keystroke/filter change bumps it; a
    /// result is rendered only if its seq still matches (latest-wins).
    pub(crate) search_seq: Cell<u64>,
    /// Sender into the background library-query worker (own read connection), used
    /// for browse/detail/play queries so they never block the GTK main loop.
    pub(crate) lib_tx: async_channel::Sender<crate::ui::libworker::LibJob>,
    /// Monotonic library-job generation (every submitted job gets a unique seq).
    pub(crate) lib_seq: Cell<u64>,
    /// The latest *render* (navigation) job's seq — a render result is applied
    /// only if it still matches, dropping superseded tab/detail switches.
    pub(crate) lib_render_seq: Cell<u64>,
    /// Continuations awaiting their worker result, keyed by job seq.
    pub(crate) lib_pending: RefCell<crate::ui::libworker::Pending>,
    // queue
    pub(crate) queue_box: gtk::Box,
    pub(crate) queue_title: gtk::Label,
    /// Async, cached cover-art textures (decoded off the main thread).
    pub(crate) art: ArtCache,
}

pub(crate) type SharedState = Rc<RefCell<State>>;
pub(crate) type SharedUi = Rc<Ui>;

impl Ui {
    pub(crate) fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }
}

impl State {
    /// Stash the album list so flow-box activation can resolve an index.
    pub(crate) fn queue_albums(&mut self, albums: Vec<Album>) {
        self.albums = albums;
    }
}
