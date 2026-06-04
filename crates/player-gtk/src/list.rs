//! `GtkListView` / `GtkGridView` factories backed by a `gio::ListStore` of
//! `glib::BoxedAnyObject`. Only the rows in (or near) the viewport are realised
//! — cell recycling + viewport culling — so browsing a 100k-track library no
//! longer builds 100k widgets up front. Each item type (`Track`, `Album`,
//! `Artist`, `Folder`) is stashed in a `BoxedAnyObject` and the row/cell widget
//! is built per bind from the borrowed value.

use std::rc::Rc;

use gtk4 as gtk;

use gtk::prelude::*;
use gtk::{gio, glib};

fn store_of<T: Clone + 'static>(items: &[T]) -> gio::ListStore {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    for it in items {
        store.append(&glib::BoxedAnyObject::new(it.clone()));
    }
    store
}

fn bind_factory<T, MakeRow>(make_row: MakeRow) -> gtk::SignalListItemFactory
where
    T: 'static,
    MakeRow: Fn(&T) -> gtk::Widget + 'static,
{
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let Some(obj) = item.item().and_downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        let value = obj.borrow::<T>();
        item.set_child(Some(&make_row(&value)));
    });
    // Drop the child when a row scrolls out so a recycled slot starts clean.
    factory.connect_unbind(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(gtk::Widget::NONE);
        }
    });
    factory
}

/// A vertical `GtkListView` over `items`. `make_row` builds one row's content
/// (called per bind, for visible rows only); `on_activate(items, index)` fires
/// on tap / Enter.
pub(crate) fn list_view<T, MakeRow, OnActivate>(
    items: Vec<T>,
    make_row: MakeRow,
    on_activate: OnActivate,
) -> gtk::ListView
where
    T: Clone + 'static,
    MakeRow: Fn(&T) -> gtk::Widget + 'static,
    OnActivate: Fn(&[T], usize) + 'static,
{
    let items = Rc::new(items);
    let model = gtk::NoSelection::new(Some(store_of(&items)));
    let view = gtk::ListView::new(Some(model), Some(bind_factory(make_row)));
    view.set_single_click_activate(true);
    view.add_css_class("browse-list");
    view.connect_activate(move |_, pos| on_activate(&items, pos as usize));
    view
}

/// A `GtkGridView` with a fixed column count over `items` (used for the album
/// grid). `make_cell` builds one cell; `on_activate(items, index)` fires on tap.
pub(crate) fn grid_view<T, MakeCell, OnActivate>(
    items: Vec<T>,
    columns: u32,
    make_cell: MakeCell,
    on_activate: OnActivate,
) -> gtk::GridView
where
    T: Clone + 'static,
    MakeCell: Fn(&T) -> gtk::Widget + 'static,
    OnActivate: Fn(&[T], usize) + 'static,
{
    let items = Rc::new(items);
    let model = gtk::NoSelection::new(Some(store_of(&items)));
    let view = gtk::GridView::new(Some(model), Some(bind_factory(make_cell)));
    view.set_min_columns(columns);
    view.set_max_columns(columns);
    view.set_single_click_activate(true);
    view.add_css_class("album-grid");
    view.connect_activate(move |_, pos| on_activate(&items, pos as usize));
    view
}
