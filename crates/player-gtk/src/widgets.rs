//! Reusable, state-free widget builders for the DAP shell. These helpers take
//! plain data + callbacks and return GTK widgets; they hold no app state, so
//! they live apart from the wiring in `main.rs` to keep that file focused on
//! behaviour rather than widget plumbing.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::{Orientation, SelectionMode};
use player_library::Album;

use crate::art::{ArtCache, ArtSlot};

/// Album-art sizes used across the shell (list row · mini-player · hero).
pub(crate) const ART_ROW: i32 = 46;
pub(crate) const ART_MINI: i32 = 40;
/// Initial/fallback Now-Playing hero size; the real size is computed from the
/// window width at runtime (`ui::now_playing::hero_art_px`, stored in `Ui::hero_px`).
pub(crate) const ART_HERO: i32 = 240;

pub(crate) fn add_page(
    stack: &adw::ViewStack,
    child: &impl IsA<gtk::Widget>,
    name: &str,
    title: &str,
    icon: &str,
) {
    let page = stack.add_titled(child, Some(name), title);
    page.set_icon_name(Some(icon));
}

pub(crate) fn circle(icon: &str, size: i32, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("circular");
    b.add_css_class("flat");
    b.set_size_request(size, size);
    b.set_tooltip_text(Some(tip));
    b
}

/// A track row's optional heart toggle: the initial loved state plus a callback
/// invoked with the new state when tapped. Boxed so the row builders can stay
/// non-generic over it (and so non-track rows can pass `None`).
pub(crate) type Heart = Option<(bool, Box<dyn Fn(bool)>)>;

/// A recycled row's re-targetable button callbacks: the per-item closure is
/// swapped in on each bind (the buttons are built once in [`row_setup`]).
/// `HeartCb` receives the new loved state; `ActionCb` is the trailing tap.
type HeartCb = Rc<RefCell<Option<Box<dyn Fn(bool)>>>>;
type ActionCb = Rc<RefCell<Option<Box<dyn Fn()>>>>;

/// A row's optional trailing button as applied on bind: icon name, tooltip, tap.
type Trailing<'a> = Option<(&'a str, &'a str, Box<dyn Fn()>)>;

/// Reflect `loved` on a heart button (filled/accented when loved, dim otherwise).
fn style_heart(b: &gtk::Button, loved: bool) {
    if loved {
        b.add_css_class("loved");
    } else {
        b.remove_css_class("loved");
    }
    b.set_tooltip_text(Some(if loved { "Loved — tap to remove" } else { "Love this track" }));
}

/// Build a heart toggle that flips its own style on click and reports the new
/// state to `on_toggle` (which persists it).
fn heart_button(loved: bool, on_toggle: Box<dyn Fn(bool)>) -> gtk::Button {
    let b = circle("emblem-favorite-symbolic", 32, "");
    style_heart(&b, loved);
    let bw = b.clone();
    b.connect_clicked(move |_| {
        let now = !bw.has_css_class("loved");
        style_heart(&bw, now);
        on_toggle(now);
    });
    b
}

pub(crate) fn flat_menu_item(icon: &str, label: &str) -> gtk::Button {
    let b = gtk::Button::new();
    b.add_css_class("flat");
    let row = gtk::Box::new(Orientation::Horizontal, 10);
    let i = gtk::Image::from_icon_name(icon);
    let l = gtk::Label::new(Some(label));
    l.set_xalign(0.0);
    l.set_hexpand(true);
    row.append(&i);
    row.append(&l);
    b.set_child(Some(&row));
    b
}

pub(crate) fn section_label(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(&text.to_uppercase()));
    l.add_css_class("section-label");
    l.set_xalign(0.0);
    l
}

pub(crate) fn boxed_list() -> gtk::ListBox {
    let lb = gtk::ListBox::new();
    lb.add_css_class("boxed-list");
    lb.set_selection_mode(SelectionMode::None);
    lb.set_margin_bottom(16);
    lb
}

/// The inner content of a list row: art · title/subtitle · mono meta · optional
/// trailing button. No row-activation gesture — the caller decides how taps are
/// handled (a `ListView`/`GridView` factory lets the view drive activation; the
/// `ListBox` path wraps this in [`row_widget`] which adds its own gesture). The
/// trailing circle button always claims its own clicks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn row_inner(
    cache: &ArtCache,
    art_hash: Option<&str>,
    title: &str,
    subtitle: &str,
    meta: Option<&str>,
    heart: Heart,
    trailing: Option<(&str, &str)>,
    on_trailing: impl Fn() + 'static,
) -> gtk::Box {
    let row = gtk::Box::new(Orientation::Horizontal, 13);
    row.set_margin_top(8);
    row.set_margin_bottom(8);
    row.set_margin_start(12);
    row.set_margin_end(8);
    row.append(&art_widget(cache, art_hash, ART_ROW, false));

    let texts = gtk::Box::new(Orientation::Vertical, 1);
    texts.set_hexpand(true);
    texts.set_valign(gtk::Align::Center);
    let t = gtk::Label::new(Some(title));
    t.add_css_class("heading");
    t.set_xalign(0.0);
    t.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let s = gtk::Label::new(Some(subtitle));
    s.add_css_class("dim-label");
    s.set_xalign(0.0);
    s.set_ellipsize(gtk::pango::EllipsizeMode::End);
    texts.append(&t);
    texts.append(&s);
    row.append(&texts);

    if let Some(m) = meta {
        let ml = gtk::Label::new(Some(m));
        ml.add_css_class("dim-label");
        ml.add_css_class("mono");
        ml.add_css_class("caption");
        // Ellipsize so a long spec ("3:46 · FLAC · 44.1 kHz · 24-bit") can't pin a
        // row (and thus the page) wider than a phone screen.
        ml.set_ellipsize(gtk::pango::EllipsizeMode::End);
        row.append(&ml);
    }

    if let Some((loved, on_toggle)) = heart {
        row.append(&heart_button(loved, on_toggle));
    }

    if let Some((icon, tip)) = trailing {
        let b = circle(icon, 32, tip);
        b.connect_clicked(move |_| on_trailing());
        row.append(&b);
    }
    row
}

/// A boxed-list row (for `GtkListBox`): [`row_inner`] plus a `GestureClick` for
/// taps and Enter-key activation. (A `ListBoxRow::activate` is a keyboard-only
/// action signal — driving taps with the gesture was the original
/// "touching a track does nothing" fix.)
#[allow(clippy::too_many_arguments)]
pub(crate) fn row_widget(
    cache: &ArtCache,
    art_hash: Option<&str>,
    title: &str,
    subtitle: &str,
    meta: Option<&str>,
    heart: Heart,
    trailing: Option<(&str, &str)>,
    on_activate: impl Fn() + 'static,
    on_trailing: impl Fn() + 'static,
) -> gtk::ListBoxRow {
    let content = row_inner(cache, art_hash, title, subtitle, meta, heart, trailing, on_trailing);
    let on_activate = Rc::new(on_activate);
    let lbr = gtk::ListBoxRow::new();
    lbr.set_child(Some(&content));
    lbr.set_activatable(true);
    {
        let on_activate = on_activate.clone();
        lbr.connect_activate(move |_| on_activate());
    }
    let tap = gtk::GestureClick::new();
    tap.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        on_activate();
    });
    lbr.add_controller(tap);
    lbr
}

/// The reusable widgets of one virtualised list row (art · title/subtitle · mono
/// meta · heart · trailing button), updated per [`list`](crate::list) bind. Built
/// once by [`row_setup`]; the `set_*` helpers re-point it at a new item (text, art
/// paintable, and the per-row heart/trailing callbacks) without rebuilding the
/// subtree. The static `GtkListBox` path still uses [`row_inner`].
pub(crate) struct RowHandle {
    art: ArtSlot,
    title: gtk::Label,
    subtitle: gtk::Label,
    meta: gtk::Label,
    heart: gtk::Button,
    heart_cb: HeartCb,
    trailing: gtk::Button,
    trailing_cb: ActionCb,
}

/// Build one reusable list row for a list factory's `setup`. All optional bits
/// (meta · heart · trailing) start hidden and are revealed per bind.
pub(crate) fn row_setup() -> (gtk::Widget, Rc<RowHandle>) {
    let row = gtk::Box::new(Orientation::Horizontal, 13);
    row.set_margin_top(8);
    row.set_margin_bottom(8);
    row.set_margin_start(12);
    row.set_margin_end(8);

    let art = ArtSlot::new(ART_ROW, false);
    row.append(&art.widget());

    let texts = gtk::Box::new(Orientation::Vertical, 1);
    texts.set_hexpand(true);
    texts.set_valign(gtk::Align::Center);
    let title = gtk::Label::new(None);
    title.add_css_class("heading");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let subtitle = gtk::Label::new(None);
    subtitle.add_css_class("dim-label");
    subtitle.set_xalign(0.0);
    subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
    texts.append(&title);
    texts.append(&subtitle);
    row.append(&texts);

    // Ellipsize so a long spec ("3:46 · FLAC · 44.1 kHz · 24-bit") can't pin a
    // row (and thus the page) wider than a phone screen.
    let meta = gtk::Label::new(None);
    meta.add_css_class("dim-label");
    meta.add_css_class("mono");
    meta.add_css_class("caption");
    meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
    meta.set_visible(false);
    row.append(&meta);

    let heart = circle("emblem-favorite-symbolic", 32, "");
    heart.set_visible(false);
    let heart_cb: HeartCb = Rc::new(RefCell::new(None));
    {
        let (hb, cb) = (heart.clone(), heart_cb.clone());
        heart.connect_clicked(move |_| {
            let now = !hb.has_css_class("loved");
            style_heart(&hb, now);
            if let Some(f) = cb.borrow().as_ref() {
                f(now);
            }
        });
    }
    row.append(&heart);

    let trailing = circle("media-playback-start-symbolic", 32, "");
    trailing.set_visible(false);
    let trailing_cb: ActionCb = Rc::new(RefCell::new(None));
    {
        let cb = trailing_cb.clone();
        trailing.connect_clicked(move |_| {
            if let Some(f) = cb.borrow().as_ref() {
                f();
            }
        });
    }
    row.append(&trailing);

    let handle = Rc::new(RowHandle {
        art,
        title,
        subtitle,
        meta,
        heart,
        heart_cb,
        trailing,
        trailing_cb,
    });
    (row.upcast(), handle)
}

impl RowHandle {
    pub(crate) fn set_art(&self, hash: Option<&str>, cache: &ArtCache) {
        self.art.set(hash, cache);
    }

    pub(crate) fn set_texts(&self, title: &str, subtitle: &str, meta: Option<&str>) {
        self.title.set_label(title);
        self.subtitle.set_label(subtitle);
        match meta {
            Some(m) => {
                self.meta.set_label(m);
                self.meta.set_visible(true);
            }
            None => self.meta.set_visible(false),
        }
    }

    pub(crate) fn set_heart(&self, heart: Heart) {
        match heart {
            Some((loved, on_toggle)) => {
                style_heart(&self.heart, loved);
                *self.heart_cb.borrow_mut() = Some(on_toggle);
                self.heart.set_visible(true);
            }
            None => {
                *self.heart_cb.borrow_mut() = None;
                self.heart.set_visible(false);
            }
        }
    }

    pub(crate) fn set_trailing(&self, trailing: Trailing) {
        match trailing {
            Some((icon, tip, on_activate)) => {
                self.trailing.set_icon_name(icon);
                self.trailing.set_tooltip_text(Some(tip));
                *self.trailing_cb.borrow_mut() = Some(on_activate);
                self.trailing.set_visible(true);
            }
            None => {
                *self.trailing_cb.borrow_mut() = None;
                self.trailing.set_visible(false);
            }
        }
    }
}

/// The reusable widgets of one album-grid cell, updated per [`list`](crate::list)
/// bind. Built once by [`album_cell_setup`]; [`album_cell_bind`] re-points it at a
/// new album (text + art paintable) without rebuilding the subtree.
pub(crate) struct AlbumCellHandle {
    art: ArtSlot,
    title: gtk::Label,
    artist: gtk::Label,
}

/// Build one reusable album cell at `art_px` (square cover). Returns the cell
/// widget and its handle for the grid factory's `setup`.
pub(crate) fn album_cell_setup(art_px: i32) -> (gtk::Widget, Rc<AlbumCellHandle>) {
    let cell = gtk::Box::new(Orientation::Vertical, 6);
    cell.add_css_class("album-cell");
    let art = ArtSlot::new(art_px, false);
    cell.append(&art.widget());
    let title = grid_label("heading");
    let artist = grid_label("caption");
    artist.add_css_class("dim-label");
    cell.append(&title);
    cell.append(&artist);
    (cell.upcast(), Rc::new(AlbumCellHandle { art, title, artist }))
}

/// Apply `al` to a cell built by [`album_cell_setup`] (the grid factory's `bind`).
pub(crate) fn album_cell_bind(h: &AlbumCellHandle, al: &Album, cache: &ArtCache) {
    h.art.set(al.art_hash.as_deref(), cache);
    h.title.set_label(&al.album);
    h.artist.set_label(al.album_artist.as_deref().unwrap_or("Unknown Artist"));
}

fn grid_label(css: &str) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.add_css_class(css);
    l.set_xalign(0.0);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l.set_max_width_chars(10);
    l
}

/// A one-shot square cover for non-recycled placements (now-playing hero, mini
/// player, album detail). Builds an [`ArtSlot`], points it at `hash`, and returns
/// the widget. (List/grid cells use a persistent [`ArtSlot`] re-targeted per bind
/// — see [`AlbumCellHandle`] / [`RowHandle`] — so they don't rebuild this.)
pub(crate) fn art_widget(cache: &ArtCache, hash: Option<&str>, size: i32, big: bool) -> gtk::Widget {
    let slot = ArtSlot::new(size, big);
    slot.set(hash, cache);
    slot.widget()
}

pub(crate) fn format_chip(spec: &str) -> gtk::Box {
    let chip = gtk::Box::new(Orientation::Horizontal, 8);
    chip.add_css_class("format-chip");
    chip.set_halign(gtk::Align::Center);
    let check = gtk::Image::from_icon_name("emblem-ok-symbolic");
    check.set_pixel_size(15);
    let label = gtk::Label::new(Some("Bit-perfect"));
    let specl = gtk::Label::new(Some(spec));
    specl.add_css_class("spec");
    // Let the spec ellipsize so the chip can shrink: a fixed-width chip would
    // otherwise pin the whole Now Playing page wider than a phone screen, and
    // AdwViewStack (sizes to its widest page) would then overflow every page.
    // Shows the full spec when it fits; trims the tail only when truly narrow.
    specl.set_ellipsize(gtk::pango::EllipsizeMode::End);
    chip.append(&check);
    chip.append(&label);
    chip.append(&specl);
    chip
}

pub(crate) fn wrap_scroller(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let s = gtk::ScrolledWindow::new();
    s.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    s.set_vexpand(true);
    s.set_child(Some(child));
    s
}

/// Centre `child` with a sane maximum width on wide windows (the libadwaita
/// recipe). No visible effect at phone width — it only kicks in once the window
/// is wider than the clamp, keeping the DAP readable on a desktop/landscape.
pub(crate) fn clamp(child: &impl IsA<gtk::Widget>) -> adw::Clamp {
    let c = adw::Clamp::new();
    c.set_maximum_size(620);
    c.set_tightening_threshold(480);
    c.set_child(Some(child));
    c
}

/// A full-page empty/placeholder state (icon · title · description), used in
/// place of hand-built `Box`es so empty views match the rest of Adwaita.
pub(crate) fn status_page(icon: &str, title: &str, description: &str) -> adw::StatusPage {
    let sp = adw::StatusPage::new();
    sp.set_icon_name(Some(icon));
    sp.set_title(title);
    sp.set_description(Some(description));
    sp
}

pub(crate) fn fill(container: &gtk::Box, child: &impl IsA<gtk::Widget>) {
    while let Some(c) = container.first_child() {
        container.remove(&c);
    }
    container.append(child);
}

pub(crate) fn toggle_accent(b: &gtk::Button, on: bool) {
    if on {
        b.add_css_class("accent");
    } else {
        b.remove_css_class("accent");
    }
}

pub(crate) fn mmss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}
