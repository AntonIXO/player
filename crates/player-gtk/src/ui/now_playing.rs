//! The Playing page: hero art, titles, transport, the bit-perfect format chip,
//! and the now-playing display helpers that keep it (and the mini bar) in sync.

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::Orientation;
use player_library::{fmt_dur_ms, Track};

use crate::state::{SharedState, SharedUi};
use crate::widgets::{
    art_widget, circle, clamp, fill, format_chip, status_page, ART_HERO, ART_MINI,
};

pub(crate) struct Np {
    pub(crate) art: gtk::Box,
    pub(crate) title: gtk::Label,
    pub(crate) subtitle: gtk::Label,
    pub(crate) elapsed: gtk::Label,
    pub(crate) total: gtk::Label,
    pub(crate) seek: gtk::Scale,
    pub(crate) play: gtk::Button,
    pub(crate) prev: gtk::Button,
    pub(crate) next: gtk::Button,
    pub(crate) rewind: gtk::Button,
    pub(crate) fwd: gtk::Button,
    pub(crate) shuffle: gtk::Button,
    pub(crate) repeat: gtk::Button,
    pub(crate) love: gtk::Button,
    pub(crate) goto_artist: gtk::Button,
    pub(crate) goto_album: gtk::Button,
    pub(crate) format: gtk::Box,
    pub(crate) stack: gtk::Stack,
}

pub(crate) fn build_now_playing() -> (gtk::Widget, Np) {
    let art = gtk::Box::new(Orientation::Vertical, 0);
    art.set_halign(gtk::Align::Center);
    art.set_size_request(ART_HERO, ART_HERO);

    let title = gtk::Label::new(Some("No Track Playing"));
    title.add_css_class("title-1");
    title.set_wrap(true);
    title.set_justify(gtk::Justification::Center);
    let subtitle = gtk::Label::new(None);
    subtitle.add_css_class("dim-label");
    subtitle.set_wrap(true);
    subtitle.set_justify(gtk::Justification::Center);

    // seek + times
    let seek = gtk::Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 0.001);
    seek.set_draw_value(false);
    seek.set_hexpand(true);
    seek.set_tooltip_text(Some("Drag to seek"));
    let elapsed = gtk::Label::new(Some("0:00"));
    elapsed.add_css_class("mono");
    elapsed.add_css_class("dim-label");
    let total = gtk::Label::new(Some("0:00"));
    total.add_css_class("mono");
    total.add_css_class("dim-label");
    let times = gtk::Box::new(Orientation::Horizontal, 0);
    times.append(&elapsed);
    let spacer = gtk::Box::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    times.append(&spacer);
    times.append(&total);

    // secondary toggle row — shuffle / repeat as pills, above the seek bar
    // (Poweramp-Adwaita design: the transport row is reserved for playback).
    let shuffle = circle("media-playlist-shuffle-symbolic", 40, "Shuffle");
    shuffle.add_css_class("pill-toggle");
    let repeat = circle("media-playlist-repeat-symbolic", 40, "Repeat");
    repeat.add_css_class("pill-toggle");
    // Heart the current track (filled/accented when loved). Disabled until a
    // library-indexed track is playing.
    let love = circle("emblem-favorite-symbolic", 40, "Love this track");
    love.add_css_class("pill-toggle");
    love.set_sensitive(false);
    // Jump to the current track's artist / album detail page.
    let goto_artist = circle("avatar-default-symbolic", 40, "Go to artist");
    goto_artist.add_css_class("pill-toggle");
    goto_artist.set_sensitive(false);
    let goto_album = circle("media-optical-symbolic", 40, "Go to album");
    goto_album.add_css_class("pill-toggle");
    goto_album.set_sensitive(false);
    let toggles = gtk::Box::new(Orientation::Horizontal, 9);
    toggles.set_halign(gtk::Align::Center);
    toggles.append(&shuffle);
    toggles.append(&repeat);
    toggles.append(&love);
    toggles.append(&goto_artist);
    toggles.append(&goto_album);

    // transport: prev · −10s · play · +10s · next
    let prev = circle("media-skip-backward-symbolic", 48, "Previous");
    let rewind = circle("media-seek-backward-symbolic", 48, "Back 10 seconds");
    let play = circle("media-playback-start-symbolic", 72, "Play");
    play.add_css_class("play-hero");
    play.remove_css_class("flat");
    let fwd = circle("media-seek-forward-symbolic", 48, "Forward 10 seconds");
    let next = circle("media-skip-forward-symbolic", 48, "Next");
    let transport = gtk::Box::new(Orientation::Horizontal, 4);
    transport.set_halign(gtk::Align::Center);
    for b in [&prev, &rewind, &play, &fwd, &next] {
        transport.append(b);
    }

    let format = gtk::Box::new(Orientation::Horizontal, 8);
    format.set_halign(gtk::Align::Center);

    // A single centred column (matching the design's NowPlayingView): hero art,
    // titles, toggles, seek, transport, format chip — stacked with an airy rhythm
    // and centred vertically in the page (see the `clamp` valign below). No
    // dock-to-bottom spacer: the device screen is tall, so a spacer just left a
    // big empty gap with a lonely chip at the bottom.
    let content = gtk::Box::new(Orientation::Vertical, 14);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.append(&art);
    let titles = gtk::Box::new(Orientation::Vertical, 3);
    titles.append(&title);
    titles.append(&subtitle);
    content.append(&titles);
    content.append(&toggles);
    let seekbox = gtk::Box::new(Orientation::Vertical, 4);
    seekbox.append(&seek);
    seekbox.append(&times);
    content.append(&seekbox);
    content.append(&transport);
    content.append(&format);

    // empty state — a standard Adwaita status page.
    let empty = status_page(
        "folder-music-symbolic",
        "No Track Playing",
        "Open a file or pick something from your library.",
    );

    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    // Centre the cluster vertically in the page (`valign: Center`, no vexpand) so it
    // sits balanced on the tall phone screen. The `clamp` caps the width so the seek
    // bar/hero stay readable on a wide desktop window. **No scroller** — the device
    // screen is tall enough for the cluster (hero ≤300 + transport ≈ 645 px, well
    // under the height budget), and the page must never scroll.
    let content_page = clamp(&content);
    content_page.set_valign(gtk::Align::Center);
    stack.add_named(&content_page, Some("content"));
    stack.set_visible_child_name("empty");

    (
        stack.clone().upcast(),
        Np {
            art,
            title,
            subtitle,
            elapsed,
            total,
            seek,
            play,
            prev,
            next,
            rewind,
            fwd,
            shuffle,
            repeat,
            love,
            goto_artist,
            goto_album,
            format,
            stack: stack.clone(),
        },
    )
}

pub(crate) fn show_track(_state: &SharedState, ui: &SharedUi, track: &Track) {
    ui.title.set_subtitle(&format!("▶ {}", track.display_title()));
    ui.np_title.set_label(&track.display_title());
    ui.np_subtitle.set_label(&track.subtitle());
    ui.np_total
        .set_label(&fmt_dur_ms(track.duration_ms.unwrap_or(0)));
    ui.np_elapsed.set_label("0:00");
    ui.np_seek.set_value(0.0);
    ui.mp_progress.set_fraction(0.0);
    // Size the hero from the (possibly wider-than-360) window; falls back to the
    // build-time default until the surface settles and `resize_hero` runs.
    let hero = ui.hero_px.get();
    ui.np_art.set_size_request(hero, hero);
    fill(&ui.np_art, &art_widget(&ui.art, track.art_hash.as_deref(), hero, true));
    fill(&ui.mp_art, &art_widget(&ui.art, track.art_hash.as_deref(), ART_MINI, false));
    ui.mp_title.set_label(&track.display_title());
    ui.mp_artist.set_label(&track.subtitle());
    fill(&ui.np_format, &format_chip(&track.signal_spec()));
    let has_artist = track
        .album_artist
        .as_deref()
        .or(track.artist.as_deref())
        .is_some_and(|a| !a.is_empty());
    let has_album = track.album.as_deref().is_some_and(|a| !a.is_empty());
    ui.np_goto_artist.set_sensitive(has_artist);
    ui.np_goto_album.set_sensitive(has_album);
    ui.np_love.set_sensitive(track.id > 0);
    if track.loved {
        ui.np_love.add_css_class("loved");
    } else {
        ui.np_love.remove_css_class("loved");
    }
}

pub(crate) fn update_now_playing_empty(ui: &SharedUi, empty: bool) {
    ui.np_stack
        .set_visible_child_name(if empty { "empty" } else { "content" });
}

pub(crate) fn update_mini(state: &SharedState, ui: &SharedUi) {
    let has = state.borrow().current.is_some();
    let on_playing = ui.stack.visible_child_name().as_deref() == Some("playing");
    ui.mini.set_reveal_child(has && !on_playing);
}

/// Hero-art edge length for a given window width. Scales with the screen (minus
/// the column's side margins plus extra breathing room) but is clamped so it never
/// overflows a true-360 device and never grows too large on a wide desktop window.
pub(crate) fn hero_art_px(window_w: i32) -> i32 {
    (window_w - 72).clamp(160, 300)
}

/// Recompute the Now-Playing hero size from the current window width and, if it
/// changed, resize the art box and (if a track is showing) rebuild its texture at
/// the new size. Cheap no-op when the size is unchanged, so it's safe to call on
/// every resize/poll tick.
pub(crate) fn resize_hero(state: &SharedState, ui: &SharedUi, window_w: i32) {
    let hero = hero_art_px(window_w);
    if hero == ui.hero_px.get() {
        return;
    }
    ui.hero_px.set(hero);
    ui.np_art.set_size_request(hero, hero);
    let track = {
        let s = state.borrow();
        s.current.and_then(|i| s.queue.get(i).cloned())
    };
    if let Some(t) = track {
        fill(&ui.np_art, &art_widget(&ui.art, t.art_hash.as_deref(), hero, true));
    }
}

pub(crate) fn set_play_icon(ui: &SharedUi, playing: bool) {
    let icon = if playing {
        "media-playback-pause-symbolic"
    } else {
        "media-playback-start-symbolic"
    };
    ui.np_play.set_icon_name(icon);
    ui.mp_play.set_icon_name(icon);
}
