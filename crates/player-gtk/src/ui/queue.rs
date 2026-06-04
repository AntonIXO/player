//! The Queue page: "Up Next" (the tracks after the current one) and the global
//! "Recently Played" history.

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use player_library::fmt_dur_ms;

use crate::playback::{enqueue_track, jump_to, play_track_now, remove_from_queue};
use crate::state::{SharedState, SharedUi};
use crate::widgets::{boxed_list, row_widget, section_label};

pub(crate) fn refresh_queue(state: &SharedState, ui: &SharedUi) {
    while let Some(c) = ui.queue_box.first_child() {
        ui.queue_box.remove(&c);
    }
    let (queue, current) = {
        let s = state.borrow();
        (s.queue.clone(), s.current)
    };

    // Up Next = the tracks after the current one (the playing track lives in the
    // mini-player / Playing page, not in this list). Indices are kept absolute so
    // jump/remove still address the real queue slot.
    let start = current.map(|c| c + 1).unwrap_or(0);
    let upcoming: Vec<usize> = (start..queue.len()).collect();
    ui.queue_title.set_text(&if upcoming.is_empty() {
        "Up Next".to_string()
    } else {
        format!("Up Next · {}", upcoming.len())
    });

    if upcoming.is_empty() {
        let l = gtk::Label::new(Some("Nothing queued — play an album or add tracks from search."));
        l.add_css_class("dim-label");
        l.set_wrap(true);
        l.set_margin_top(16);
        ui.queue_box.append(&l);
    } else {
        let lb = boxed_list();
        for i in upcoming {
            let t = &queue[i];
            let (s1, u1) = (state.clone(), ui.clone());
            let (s2, u2) = (state.clone(), ui.clone());
            let row = row_widget(
                &ui.art,
                t.art_hash.as_deref(),
                &t.display_title(),
                &t.subtitle(),
                None,
                Some(("list-remove-symbolic", "Remove from queue")),
                move || jump_to(&s1, &u1, i),
                move || remove_from_queue(&s2, &u2, i),
            );
            lb.append(&row);
        }
        ui.queue_box.append(&lb);
    }

    // Recently played (global history): tap to play now, ＋ to add to queue.
    let recent = state.borrow().library.recent_plays(15).unwrap_or_default();
    if !recent.is_empty() {
        let header = section_label("Recently Played");
        header.set_margin_top(16);
        ui.queue_box.append(&header);
        let lb = boxed_list();
        for t in &recent {
            let (s1, u1, t1) = (state.clone(), ui.clone(), t.clone());
            let (s2, u2, t2) = (state.clone(), ui.clone(), t.clone());
            let row = row_widget(
                &ui.art,
                t.art_hash.as_deref(),
                &t.display_title(),
                &t.subtitle(),
                Some(&fmt_dur_ms(t.duration_ms.unwrap_or(0))),
                Some(("list-add-symbolic", "Add to queue")),
                move || play_track_now(&s1, &u1, t1.clone()),
                move || enqueue_track(&s2, &u2, t2.clone()),
            );
            lb.append(&row);
        }
        ui.queue_box.append(&lb);
    }
}
