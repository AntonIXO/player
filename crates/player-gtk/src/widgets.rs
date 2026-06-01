//! Reusable, state-free widget builders for the DAP shell. These helpers take
//! plain data + callbacks and return GTK widgets; they hold no app state, so
//! they live apart from the wiring in `main.rs` to keep that file focused on
//! behaviour rather than widget plumbing.

use std::path::Path;
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::{Orientation, SelectionMode};
use player_library::Album;

/// Album-art sizes used across the shell (list row · mini-player · hero).
pub(crate) const ART_ROW: i32 = 46;
pub(crate) const ART_MINI: i32 = 40;
pub(crate) const ART_HERO: i32 = 220;

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

/// A vertically-stacked, padded container for a browse-tab list.
pub(crate) fn list_tab_box() -> gtk::Box {
    let b = gtk::Box::new(Orientation::Vertical, 0);
    b.set_margin_top(8);
    b.set_margin_start(16);
    b.set_margin_end(16);
    b.set_margin_bottom(20);
    b
}

/// A boxed-list row: art · title/subtitle · mono meta · optional trailing button.
#[allow(clippy::too_many_arguments)]
pub(crate) fn row_widget(
    art_dir: &Path,
    art_hash: Option<&str>,
    title: &str,
    subtitle: &str,
    meta: Option<&str>,
    trailing: Option<(&str, &str)>,
    on_activate: impl Fn() + 'static,
    on_trailing: impl Fn() + 'static,
) -> gtk::ListBoxRow {
    let row = gtk::Box::new(Orientation::Horizontal, 13);
    row.set_margin_top(8);
    row.set_margin_bottom(8);
    row.set_margin_start(12);
    row.set_margin_end(8);
    row.append(&art_widget(art_dir, art_hash, ART_ROW, false));

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

    if let Some((icon, tip)) = trailing {
        let b = circle(icon, 32, tip);
        b.connect_clicked(move |_| on_trailing());
        row.append(&b);
    }

    let lbr = gtk::ListBoxRow::new();
    lbr.set_child(Some(&row));
    lbr.set_activatable(true);
    // `ListBoxRow::activate` is a *keyboard* action signal — it does NOT fire on a
    // pointer tap (that was the "touching a track does nothing" bug). Drive taps
    // with an explicit `GestureClick` and keep `connect_activate` for the Enter
    // key. The trailing circle button claims its own clicks, so tapping it won't
    // also fire the row gesture.
    let on_activate = Rc::new(on_activate);
    lbr.connect_activate({
        let on_activate = on_activate.clone();
        move |_| on_activate()
    });
    let tap = gtk::GestureClick::new();
    tap.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        on_activate();
    });
    lbr.add_controller(tap);
    lbr
}

pub(crate) fn album_cell(art_dir: &Path, al: &Album) -> gtk::Box {
    let cell = gtk::Box::new(Orientation::Vertical, 6);
    cell.add_css_class("album-cell");
    let art = art_widget(art_dir, al.art_hash.as_deref(), 110, false);
    art.set_size_request(110, 110);
    cell.append(&art);
    let t = gtk::Label::new(Some(&al.album));
    t.add_css_class("heading");
    t.set_xalign(0.0);
    t.set_ellipsize(gtk::pango::EllipsizeMode::End);
    t.set_max_width_chars(14);
    let a = gtk::Label::new(Some(al.album_artist.as_deref().unwrap_or("Unknown Artist")));
    a.add_css_class("dim-label");
    a.add_css_class("caption");
    a.set_xalign(0.0);
    a.set_ellipsize(gtk::pango::EllipsizeMode::End);
    a.set_max_width_chars(14);
    cell.append(&t);
    cell.append(&a);
    cell
}

pub(crate) fn art_widget(art_dir: &Path, hash: Option<&str>, size: i32, big: bool) -> gtk::Widget {
    // A fixed square `Box` with `overflow: hidden` + the `.art` radius clips its
    // child to rounded corners (the standard GTK4 recipe — an `AspectFrame` does
    // not). The art must be an *exact* `size × size` square: a bare `Picture`
    // reports its source pixels as its natural size, so in a vertically- or
    // horizontally-unconstrained parent (the now-playing hero, the mini-player
    // bar) it balloons past `size` — `set_size_request` is only a *minimum*.
    // Pinning min == max via `gtk::SizeGroup`-style fixed sizing isn't available,
    // so we wrap the cover in an `AspectFrame`-free fixed box and constrain the
    // `Picture` itself to the same request with non-expanding `Fill` alignment;
    // the overflow-hidden frame then crops the Cover-fit overscan to the square.
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
    match hash {
        Some(h) if art_dir.join(h).exists() => {
            // A `gtk::Image` backed by the file paints at exactly `pixel_size`
            // (it never reports a larger natural size the way `Picture` does), so
            // the square is honoured in every container. Covers are square, so
            // the contain-fit matches the previous cover-crop visually.
            let img = gtk::Image::from_file(art_dir.join(h));
            img.set_pixel_size(size);
            img.set_hexpand(true);
            img.set_vexpand(true);
            frame.append(&img);
        }
        _ => {
            frame.add_css_class("art-placeholder");
            let icon = gtk::Image::from_icon_name("folder-music-symbolic");
            icon.set_pixel_size((size as f32 * 0.42) as i32);
            icon.add_css_class("dim-label");
            icon.set_hexpand(true);
            icon.set_vexpand(true);
            frame.append(&icon);
        }
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
