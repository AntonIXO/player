//! Reusable, state-free widget builders for the DAP shell. These helpers take
//! plain data + callbacks and return GTK widgets; they hold no app state, so
//! they live apart from the wiring in `main.rs` to keep that file focused on
//! behaviour rather than widget plumbing.

use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::{Orientation, SelectionMode};
use player_library::Album;

use crate::art::ArtCache;

/// Album-art sizes used across the shell (list row · mini-player · hero).
pub(crate) const ART_ROW: i32 = 46;
pub(crate) const ART_MINI: i32 = 40;
pub(crate) const ART_HERO: i32 = 264;

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

pub(crate) fn album_cell(cache: &ArtCache, al: &Album, art_px: i32) -> gtk::Box {
    let cell = gtk::Box::new(Orientation::Vertical, 6);
    cell.add_css_class("album-cell");
    let art = art_widget(cache, al.art_hash.as_deref(), art_px, false);
    art.set_size_request(art_px, art_px);
    cell.append(&art);
    let t = gtk::Label::new(Some(&al.album));
    t.add_css_class("heading");
    t.set_xalign(0.0);
    t.set_ellipsize(gtk::pango::EllipsizeMode::End);
    t.set_max_width_chars(10);
    let a = gtk::Label::new(Some(al.album_artist.as_deref().unwrap_or("Unknown Artist")));
    a.add_css_class("dim-label");
    a.add_css_class("caption");
    a.set_xalign(0.0);
    a.set_ellipsize(gtk::pango::EllipsizeMode::End);
    a.set_max_width_chars(10);
    cell.append(&t);
    cell.append(&a);
    cell
}

pub(crate) fn art_widget(cache: &ArtCache, hash: Option<&str>, size: i32, big: bool) -> gtk::Widget {
    // A fixed square `Box` with `overflow: hidden` + the `.art` radius clips its
    // child to rounded corners (the standard GTK4 recipe — an `AspectFrame` does
    // not). The cover is shown via a `gtk::Image`, which paints a paintable at
    // exactly `pixel_size` (a bare `Picture` reports its source pixels as its
    // natural size and balloons past `size` in an unconstrained parent like the
    // now-playing hero / mini-player). The texture is loaded asynchronously
    // through the cache; a placeholder icon shows until it arrives.
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
    let icon = gtk::Image::from_icon_name("folder-music-symbolic");
    icon.set_pixel_size((size as f32 * 0.42) as i32);
    icon.add_css_class("dim-label");
    icon.set_hexpand(true);
    icon.set_vexpand(true);
    frame.append(&icon);

    if let Some(h) = hash {
        let frame_weak = frame.downgrade();
        cache.request(h, size, move |tex| {
            let Some(frame) = frame_weak.upgrade() else { return };
            while let Some(c) = frame.first_child() {
                frame.remove(&c);
            }
            frame.remove_css_class("art-placeholder");
            let img = gtk::Image::from_paintable(Some(tex));
            img.set_pixel_size(size);
            img.set_hexpand(true);
            img.set_vexpand(true);
            frame.append(&img);
        });
    }
    frame.upcast()
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
