//! Async, cached album-art textures.
//!
//! Replaces the synchronous `gtk::Image::from_file` calls that read + decoded
//! covers on the GTK main thread (in the frame path, per row). Covers are
//! decoded and scaled to the display size on a worker thread; the main loop
//! only wraps the result in a GPU `gdk::MemoryTexture` and caches it by
//! `(hash, size)`. Repeat requests for the same cover are coalesced, and a
//! cache hit resolves synchronously so already-seen art appears instantly.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4 as gtk;

use gtk::gdk;
use gtk::gdk_pixbuf;
use gtk::glib;
use gtk::prelude::*;

type Key = (String, i32);
type Callback = Box<dyn Fn(&gdk::Texture)>;

struct Inner {
    art_dir: PathBuf,
    cache: RefCell<HashMap<Key, gdk::Texture>>,
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
                cache: RefCell::new(HashMap::new()),
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
        if let Some(tex) = self.inner.cache.borrow().get(&key) {
            cb(tex);
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
