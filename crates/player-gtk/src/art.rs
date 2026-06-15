//! Async, cached album-art textures and the reusable [`ArtSlot`] art widget.
//!
//! Replaces the synchronous `gtk::Image::from_file` calls that read + decoded
//! covers on the GTK main thread (in the frame path, per row). Covers are
//! decoded and scaled to the display size on a worker thread; the main loop
//! only wraps the result in a GPU `gdk::MemoryTexture` and caches it by
//! `(hash, size)`. Repeat requests for the same cover are coalesced, and a
//! cache hit resolves synchronously so already-seen art appears instantly.
//!
//! [`ArtSlot`] is the per-cell art widget: a persistent frame + placeholder/cover
//! `Image` pair that is built **once** and re-targeted on each list bind (swap the
//! paintable, don't rebuild the subtree), so `GtkGridView`/`GtkListView` cell
//! recycling actually recycles. A monotonic generation token drops the result of
//! a decode whose cell has since been rebound to another album.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4 as gtk;

use gtk::gdk;
use gtk::gdk_pixbuf;
use gtk::glib;
use gtk::prelude::*;
use gtk::Orientation;

type Key = (String, i32);
type Callback = Box<dyn Fn(&gdk::Texture)>;

/// Max number of decoded `(hash, size)` textures kept resident. Covers are
/// decoded per display size, and the resize path mints new size variants, so the
/// map would otherwise grow without bound — a real concern on the 3GB phone. The
/// realised list/grid only ever shows ~viewport-worth of cells, so a few hundred
/// entries comfortably covers scroll-back without re-decoding.
const ART_CACHE_CAP: usize = 256;

/// A tiny capacity-bounded LRU over decoded textures. Eviction is glitch-free: a
/// displayed `gdk::Texture` is refcounted and the `GtkImage` holds its own ref,
/// so dropping a cache entry only costs a future re-decode, never a visible pop.
struct LruCache {
    map: HashMap<Key, (gdk::Texture, u64)>,
    tick: u64,
    cap: usize,
}

impl LruCache {
    fn new(cap: usize) -> Self {
        LruCache { map: HashMap::new(), tick: 0, cap }
    }

    fn get(&mut self, key: &Key) -> Option<gdk::Texture> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|e| {
            e.1 = tick;
            e.0.clone()
        })
    }

    fn insert(&mut self, key: Key, tex: gdk::Texture) {
        self.tick += 1;
        if !self.map.contains_key(&key) && self.map.len() >= self.cap {
            if let Some(lru) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&lru);
            }
        }
        self.map.insert(key, (tex, self.tick));
    }
}

struct Inner {
    art_dir: PathBuf,
    cache: RefCell<LruCache>,
    pending: RefCell<HashMap<Key, Vec<Callback>>>,
}

/// A cheap-to-clone handle to the shared, main-thread art cache.
#[derive(Clone)]
pub(crate) struct ArtCache {
    inner: Rc<Inner>,
}

/// Raw decoded pixels handed back from the worker thread — every field is
/// `Send` (`glib::Bytes` is reference-counted immutable data), so the `Pixbuf`
/// itself (which is `!Send`) never crosses the thread boundary.
struct Decoded {
    bytes: glib::Bytes,
    width: i32,
    height: i32,
    rowstride: i32,
    has_alpha: bool,
}

impl ArtCache {
    pub(crate) fn new(art_dir: PathBuf) -> Self {
        ArtCache {
            inner: Rc::new(Inner {
                art_dir,
                cache: RefCell::new(LruCache::new(ART_CACHE_CAP)),
                pending: RefCell::new(HashMap::new()),
            }),
        }
    }

    /// Request the texture for `hash` rendered at `size`px. On a cache hit `cb`
    /// runs synchronously; otherwise the cover is decoded off-thread and `cb`
    /// runs on the main loop when ready (the caller shows a placeholder until
    /// then). Concurrent requests for the same key are coalesced into one load.
    pub(crate) fn request(&self, hash: &str, size: i32, cb: impl Fn(&gdk::Texture) + 'static) {
        let key = (hash.to_string(), size);
        if let Some(tex) = self.inner.cache.borrow_mut().get(&key) {
            cb(&tex);
            return;
        }
        {
            let mut pending = self.inner.pending.borrow_mut();
            if let Some(waiters) = pending.get_mut(&key) {
                waiters.push(Box::new(cb)); // a load for this key is already in flight
                return;
            }
            pending.insert(key.clone(), vec![Box::new(cb)]);
        }
        self.spawn_load(key);
    }

    fn spawn_load(&self, key: Key) {
        let path = self.inner.art_dir.join(&key.0);
        let size = key.1;
        let (tx, rx) = async_channel::bounded::<Option<Decoded>>(1);
        std::thread::spawn(move || {
            // File read + JPEG/PNG decode + downscale all happen here, off the
            // GTK main thread. `preserve_aspect_ratio=true` fits within size².
            let decoded = gdk_pixbuf::Pixbuf::from_file_at_scale(&path, size, size, true)
                .ok()
                .map(|pb| Decoded {
                    bytes: pb.read_pixel_bytes(),
                    width: pb.width(),
                    height: pb.height(),
                    rowstride: pb.rowstride(),
                    has_alpha: pb.has_alpha(),
                });
            let _ = tx.send_blocking(decoded);
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            let decoded = rx.recv().await.ok().flatten();
            let waiters = this.inner.pending.borrow_mut().remove(&key).unwrap_or_default();
            let Some(d) = decoded else { return }; // decode failed → keep placeholders
            let format = if d.has_alpha {
                gdk::MemoryFormat::R8g8b8a8
            } else {
                gdk::MemoryFormat::R8g8b8
            };
            let tex: gdk::Texture =
                gdk::MemoryTexture::new(d.width, d.height, format, &d.bytes, d.rowstride as usize)
                    .upcast();
            this.inner.cache.borrow_mut().insert(key, tex.clone());
            for cb in waiters {
                cb(&tex);
            }
        });
    }
}

/// A reusable square album-art widget. Built once (in a list-item `setup`) and
/// re-pointed at a new cover on each `bind` via [`ArtSlot::set`] — the persistent
/// `Image` widgets and CSS classes are never rebuilt, which is what lets the
/// `GridView`/`ListView` factory recycle cells instead of re-allocating a subtree
/// per scrolled-in row.
#[derive(Clone)]
pub(crate) struct ArtSlot {
    inner: Rc<SlotInner>,
}

struct SlotInner {
    frame: gtk::Box,
    placeholder: gtk::Image,
    cover: gtk::Image,
    size: i32,
    // Bumped on every `set`; an async decode whose token no longer matches is a
    // result for a cell that has since been recycled onto a different album.
    generation: Cell<u64>,
}

impl ArtSlot {
    /// Build the art slot at `size`px (square). `big` uses the larger corner
    /// radius (`.art-lg`, for the hero/detail). Starts on the placeholder.
    pub(crate) fn new(size: i32, big: bool) -> Self {
        // A fixed square `Box` with `overflow: hidden` + the `.art` radius clips
        // its child to rounded corners (the standard GTK4 recipe — an
        // `AspectFrame` does not). The cover is shown via a `gtk::Image`, which
        // paints a paintable at exactly `pixel_size` (a bare `Picture` reports
        // its source pixels as its natural size and balloons past `size` in an
        // unconstrained parent like the now-playing hero / mini-player).
        let frame = gtk::Box::new(Orientation::Vertical, 0);
        frame.set_size_request(size, size);
        frame.set_halign(gtk::Align::Center);
        frame.set_valign(gtk::Align::Center);
        frame.set_hexpand(false);
        frame.set_vexpand(false);
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.add_css_class("art");
        if big {
            frame.add_css_class("art-lg");
        }
        frame.add_css_class("art-placeholder");

        let placeholder = gtk::Image::from_icon_name("folder-music-symbolic");
        placeholder.set_pixel_size((size as f32 * 0.42) as i32);
        placeholder.add_css_class("dim-label");
        placeholder.set_hexpand(true);
        placeholder.set_vexpand(true);
        frame.append(&placeholder);

        // The cover shares the square; only one of placeholder/cover is ever
        // visible, so toggling `visible` swaps them with no child add/remove.
        let cover = gtk::Image::new();
        cover.set_pixel_size(size);
        cover.set_hexpand(true);
        cover.set_vexpand(true);
        cover.set_visible(false);
        frame.append(&cover);

        ArtSlot {
            inner: Rc::new(SlotInner { frame, placeholder, cover, size, generation: Cell::new(0) }),
        }
    }

    /// The widget to add to a parent (the square frame).
    pub(crate) fn widget(&self) -> gtk::Widget {
        self.inner.frame.clone().upcast()
    }

    /// Point the slot at `hash` (or `None` for no art). Shows the placeholder
    /// immediately, then swaps in the cover when the async decode lands — unless
    /// the slot has been re-`set` in the meantime (recycled onto another album),
    /// in which case the stale result is dropped.
    pub(crate) fn set(&self, hash: Option<&str>, cache: &ArtCache) {
        let generation = self.inner.generation.get().wrapping_add(1);
        self.inner.generation.set(generation);
        self.show_placeholder();

        let Some(hash) = hash else { return };
        // Hold a strong ref so a one-shot caller (hero/mini/detail) that drops its
        // `ArtSlot` right after `set` still receives the cover. The ref lives only
        // until the (coalesced) decode fires its callbacks, then releases — for a
        // recycled cell the `Rc<SlotInner>` is also owned by the cell handle, so
        // there is no leak either way. Staleness is handled by the generation.
        let inner = self.inner.clone();
        cache.request(hash, self.inner.size, move |tex| {
            if inner.generation.get() != generation {
                return; // cell was recycled onto a different album mid-flight
            }
            inner.cover.set_paintable(Some(tex));
            inner.cover.set_visible(true);
            inner.placeholder.set_visible(false);
            inner.frame.remove_css_class("art-placeholder");
        });
    }

    fn show_placeholder(&self) {
        self.inner.cover.set_visible(false);
        self.inner.cover.set_paintable(gdk::Paintable::NONE);
        self.inner.placeholder.set_visible(true);
        self.inner.frame.add_css_class("art-placeholder");
    }
}
