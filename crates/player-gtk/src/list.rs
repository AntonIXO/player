//! `GtkListView` / `GtkGridView` factories backed by a `gio::ListStore` of
//! `glib::BoxedAnyObject`. Only the rows in (or near) the viewport are realised
//! — cell recycling + viewport culling — so browsing a 100k-track library no
//! longer builds 100k widgets up front.
//!
//! Cells follow GTK4's `setup` / `bind` lifecycle so recycling actually recycles:
//! `setup` builds one reusable widget template per recycled slot (and stashes a
//! typed handle on the `ListItem`), and `bind` only updates that template's data
//! for the item now scrolling in (text, art paintable, per-row callbacks). The
//! previous code rebuilt the whole cell subtree on every `bind` and tore it down
//! on `unbind`, which defeated recycling and made fast scrolling allocate ~5
//! widgets + recompute CSS per cell — the source of the scroll jank.

use std::rc::Rc;

use gtk4 as gtk;

use gtk::prelude::*;
use gtk::{gio, glib};

/// Qdata key under which each `ListItem` carries its reusable cell handle.
const HANDLE_KEY: &str = "cell-handle";

fn store_of<T: Clone + 'static>(items: &[T]) -> gio::ListStore {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    for it in items {
        store.append(&glib::BoxedAnyObject::new(it.clone()));
    }
    store
}

/// A `setup`/`bind` factory. `setup` builds the reusable cell widget once per
/// recycled slot and returns it alongside a typed handle (`Rc<H>`) holding the
/// sub-widgets to update; `bind` applies one item's data to that handle. The
/// handle is stashed on the `ListItem` via glib qdata (single-threaded, typed),
/// and is dropped automatically when the item is finalised.
fn template_factory<T, H, Setup, Bind>(setup: Setup, bind: Bind) -> gtk::SignalListItemFactory
where
    T: 'static,
    H: 'static,
    Setup: Fn() -> (gtk::Widget, Rc<H>) + 'static,
    Bind: Fn(&H, &T) + 'static,
{
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (widget, handle) = setup();
        item.set_child(Some(&widget));
        // SAFETY: main-thread only; QD type matches the `data::<Rc<H>>` read in
        // `bind`; glib drops the stored `Rc<H>` when the ListItem is finalised.
        unsafe {
            item.set_data(HANDLE_KEY, handle);
        }
    });
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(obj) = item.item().and_downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        // SAFETY: the matching `set_data::<Rc<H>>` ran in `setup` for this item.
        let Some(handle) = (unsafe { item.data::<Rc<H>>(HANDLE_KEY) }) else {
            return;
        };
        let handle = unsafe { handle.as_ref() };
        let value = obj.borrow::<T>();
        bind(handle, &value);
    });
    factory
}

/// A vertical `GtkListView` over `items`. `setup` builds one reusable row +
/// handle (visible-rows pool only); `bind(handle, item)` updates a row for the
/// item scrolling in; `on_activate(items, index)` fires on tap / Enter.
pub(crate) fn list_view<T, H, Setup, Bind, OnActivate>(
    items: Vec<T>,
    setup: Setup,
    bind: Bind,
    on_activate: OnActivate,
) -> gtk::ListView
where
    T: Clone + 'static,
    H: 'static,
    Setup: Fn() -> (gtk::Widget, Rc<H>) + 'static,
    Bind: Fn(&H, &T) + 'static,
    OnActivate: Fn(&[T], usize) + 'static,
{
    let items = Rc::new(items);
    let model = gtk::NoSelection::new(Some(store_of(&items)));
    let view = gtk::ListView::new(Some(model), Some(template_factory(setup, bind)));
    view.set_single_click_activate(true);
    view.add_css_class("browse-list");
    view.connect_activate(move |_, pos| on_activate(&items, pos as usize));
    view
}

/// A `GtkGridView` over `items` (used for the album grid). It uses between
/// `min_columns` and `max_columns` columns for the available width; pass equal
/// values to pin a fixed count. Cells must size from the column width (no fixed
/// width request) or a high `min_columns` forces the window wider than the screen.
/// `setup`/`bind` follow the same recycling contract as [`list_view`].
pub(crate) fn grid_view<T, H, Setup, Bind, OnActivate>(
    items: Vec<T>,
    min_columns: u32,
    max_columns: u32,
    setup: Setup,
    bind: Bind,
    on_activate: OnActivate,
) -> gtk::GridView
where
    T: Clone + 'static,
    H: 'static,
    Setup: Fn() -> (gtk::Widget, Rc<H>) + 'static,
    Bind: Fn(&H, &T) + 'static,
    OnActivate: Fn(&[T], usize) + 'static,
{
    let items = Rc::new(items);
    let model = gtk::NoSelection::new(Some(store_of(&items)));
    let view = gtk::GridView::new(Some(model), Some(template_factory(setup, bind)));
    view.set_min_columns(min_columns);
    view.set_max_columns(max_columns);
    view.set_single_click_activate(true);
    view.add_css_class("album-grid");
    view.connect_activate(move |_, pos| on_activate(&items, pos as usize));
    view
}
