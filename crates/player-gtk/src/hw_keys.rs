use async_channel::Sender;
use evdev::{EventSummary, KeyCode};

pub(crate) enum HwKey {
    VolumeDown,
    VolumeUp,
}

fn has_volume_key(d: &evdev::Device) -> bool {
    d.supported_keys().is_some_and(|k| {
        k.contains(KeyCode::KEY_VOLUMEDOWN) || k.contains(KeyCode::KEY_VOLUMEUP)
    })
}

/// Find every input device that advertises at least one volume key and spawn
/// one reader thread per device. On some phones (e.g. Poco F1) volume-up and
/// volume-down live on separate kernel devices, so a single-device open misses
/// one of the keys. The current thread handles the first device; extra threads
/// handle the rest.
pub(crate) fn run(tx: Sender<HwKey>) {
    let devs: Vec<evdev::Device> = evdev::enumerate()
        .filter(|(_, d)| has_volume_key(d))
        .map(|(_, d)| d)
        .collect();

    if devs.is_empty() {
        eprintln!("hw_keys: no volume-key device found");
        return;
    }

    let mut iter = devs.into_iter();
    let first = iter.next().unwrap();
    for extra in iter {
        let tx2 = tx.clone();
        std::thread::spawn(move || read_device(extra, tx2));
    }
    read_device(first, tx);
}

fn read_device(mut dev: evdev::Device, tx: Sender<HwKey>) {
    loop {
        let events = match dev.fetch_events() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("hw_keys: read error: {e}");
                return;
            }
        };
        for ev in events {
            let hw = match ev.destructure() {
                EventSummary::Key(_, code, 1) if code == KeyCode::KEY_VOLUMEDOWN => {
                    HwKey::VolumeDown
                }
                EventSummary::Key(_, code, 1) if code == KeyCode::KEY_VOLUMEUP => HwKey::VolumeUp,
                _ => continue,
            };
            if tx.send_blocking(hw).is_err() {
                return; // channel closed — app is shutting down
            }
        }
    }
}
