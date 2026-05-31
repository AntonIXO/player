//! Minimal libadwaita shell over `player-core`. Intentionally tiny — the one
//! thing worth showing for a bit-perfect player is the negotiated signal path.
//!
//! There is deliberately no volume control: the DAC's hardware knob owns volume,
//! and any software gain would break bit-perfect output.
//!
//! Set the output device with the PLAYER_DEVICE env var (default `hw:1,0`).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::{gio, glib, Orientation};
use player_core::{Event, Player, StreamSpec};

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.player.BitPerfect")
        .build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let device = std::env::var("PLAYER_DEVICE").unwrap_or_else(|_| "hw:1,0".into());

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Bit-Perfect Player")
        .default_width(460)
        .default_height(340)
        .build();

    // --- widgets ---
    let header = adw::HeaderBar::new();
    let open_btn = gtk::Button::builder()
        .label("Open")
        .css_classes(["suggested-action"])
        .build();
    header.pack_start(&open_btn);

    // Enqueue another file — gapless if it shares the current wire format.
    let add_btn = gtk::Button::with_label("Add");
    header.pack_start(&add_btn);

    let title_label = gtk::Label::builder()
        .label("No file loaded")
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(["title-2"])
        .build();

    let status_label = gtk::Label::builder().label("■ idle").build();

    let fmt_label = gtk::Label::builder()
        .label(format!("device: {device}"))
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label"])
        .build();

    let pos_label = gtk::Label::builder()
        .label("0:00")
        .css_classes(["title-1"])
        .build();

    let play_btn = gtk::Button::with_label("Play");
    let stop_btn = gtk::Button::with_label("Stop");
    let controls = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    controls.append(&play_btn);
    controls.append(&stop_btn);

    let volume_note = gtk::Label::builder()
        .label("Volume: DAC hardware only — no software volume (bit-perfect).")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label", "caption"])
        .build();

    let content = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(16)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .valign(gtk::Align::Center)
        .build();
    content.append(&title_label);
    content.append(&pos_label);
    content.append(&status_label);
    content.append(&fmt_label);
    content.append(&controls);
    content.append(&volume_note);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    // --- engine + event bridge (current glib idiom: async-channel + spawn_future_local) ---
    let (tx, rx) = async_channel::unbounded::<Event>();
    let player = Rc::new(Player::spawn(device.clone(), move |ev| {
        let _ = tx.send_blocking(ev);
    }));

    let last_path: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    let spec: Rc<RefCell<Option<StreamSpec>>> = Rc::new(RefCell::new(None));

    // Drain engine events on the GTK main thread.
    {
        let (title_label, status_label, fmt_label, pos_label, spec, device) = (
            title_label.clone(),
            status_label.clone(),
            fmt_label.clone(),
            pos_label.clone(),
            spec.clone(),
            device.clone(),
        );
        glib::spawn_future_local(async move {
            while let Ok(ev) = rx.recv().await {
                match ev {
                    Event::Started { spec: s, path } => {
                        *spec.borrow_mut() = Some(s);
                        title_label.set_label(
                            &path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string()),
                        );
                        status_label.set_label("▶ playing");
                        fmt_label.set_label(&format!(
                            "Bit-perfect ✓\n{}-bit · {} Hz · {} ch → {}\n{}",
                            s.source_bits,
                            s.rate,
                            s.channels,
                            s.fmt.label(),
                            device,
                        ));
                        pos_label.set_label("0:00");
                    }
                    Event::Position(frames) => {
                        if let Some(s) = *spec.borrow() {
                            let secs = frames / s.rate as u64;
                            pos_label.set_label(&format!("{}:{:02}", secs / 60, secs % 60));
                        }
                    }
                    Event::Ended => status_label.set_label("■ finished"),
                    Event::Error(e) => status_label.set_label(&format!("⚠ {e}")),
                }
            }
        });
    }

    // --- button wiring ---
    {
        let (player, last_path, window) = (player.clone(), last_path.clone(), window.clone());
        open_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Choose an audio file")
                .build();
            let (player, last_path) = (player.clone(), last_path.clone());
            dialog.open(Some(&window), gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        *last_path.borrow_mut() = Some(path.clone());
                        player.play(path);
                    }
                }
            });
        });
    }
    {
        let (player, window) = (player.clone(), window.clone());
        add_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Add a file to the queue")
                .build();
            let player = player.clone();
            dialog.open(Some(&window), gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        player.enqueue(path);
                    }
                }
            });
        });
    }
    {
        let (player, last_path) = (player.clone(), last_path.clone());
        play_btn.connect_clicked(move |_| {
            if let Some(p) = last_path.borrow().clone() {
                player.play(p);
            }
        });
    }
    {
        let player = player.clone();
        stop_btn.connect_clicked(move |_| player.stop());
    }

    // Keep the engine alive for the window's lifetime; dropping the last Rc on
    // window close sends Quit and joins the engine thread.
    let player_keepalive = player.clone();
    window.connect_destroy(move |_| {
        let _ = &player_keepalive;
    });

    window.present();
}
