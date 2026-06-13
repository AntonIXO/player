//! The bottom mini-player bar (art · title/artist · play · thin progress).

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::Orientation;

use crate::widgets::{circle, ART_MINI};

#[allow(clippy::type_complexity)]
pub(crate) fn build_mini(
) -> (gtk::Revealer, gtk::Box, gtk::Label, gtk::Label, gtk::Button, gtk::ProgressBar) {
    let art = gtk::Box::new(Orientation::Vertical, 0);
    art.set_size_request(ART_MINI, ART_MINI);
    let title = gtk::Label::new(None);
    title.add_css_class("heading");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let artist = gtk::Label::new(None);
    artist.add_css_class("dim-label");
    artist.add_css_class("caption");
    artist.set_xalign(0.0);
    artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let labels = gtk::Box::new(Orientation::Vertical, 0);
    labels.set_hexpand(true);
    labels.set_valign(gtk::Align::Center);
    labels.append(&title);
    labels.append(&artist);
    let play = circle("media-playback-start-symbolic", 40, "Play");

    let row = gtk::Box::new(Orientation::Horizontal, 11);
    row.set_margin_top(6);
    row.set_margin_bottom(6);
    row.set_margin_start(12);
    row.set_margin_end(12);
    row.append(&art);
    row.append(&labels);
    row.append(&play);

    let progress = gtk::ProgressBar::new();
    progress.add_css_class("mini-progress");
    progress.set_fraction(0.0);

    let bar = gtk::Box::new(Orientation::Vertical, 0);
    bar.add_css_class("toolbar");
    bar.append(&row);
    bar.append(&progress);

    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideUp);
    revealer.set_child(Some(&bar));
    revealer.set_reveal_child(false);

    (revealer, art, title, artist, play, progress)
}
