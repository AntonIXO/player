//! ALSA output-device discovery (FURTHER.md 3.1). Enumerates the **bit-perfect
//! `hw:` PCMs** — direct hardware access, no `plug`/`dmix`/`pulse` layers that
//! would resample or mix — and classifies USB DACs so the UI can default to the
//! external DAC (e.g. the Chord Mojo 2) over the phone's internal codec.
//!
//! We walk the sound cards and, per card, enumerate its playback PCM devices via
//! the control interface, synthesising `hw:CARD=<id>,DEV=<n>` names (these open
//! directly with the engine). The higher-level named PCMs that `aplay -L` shows
//! (`default`, `sysdefault`, `front`, `dmix`, `pulse`, …) are deliberately *not*
//! offered — they are not bit-perfect.

use alsa::card::Iter as CardIter;
use alsa::ctl::{Ctl, DeviceIter};
use alsa::Direction;

/// How a discovered device is connected, used to rank `auto_pick`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// A USB DAC (an external bit-perfect target — preferred).
    Usb,
    /// An on-board / internal codec.
    Internal,
    /// Anything else (HDMI, virtual, unclassified).
    Other,
}

/// A discovered ALSA output device.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// ALSA PCM name, usable directly with the engine (e.g. `hw:CARD=Mojo2,DEV=0`).
    pub id: String,
    /// Short, human name (the card name, e.g. `Chord Mojo 2`).
    pub name: String,
    /// Fuller description (card + device line).
    pub description: String,
    pub kind: DeviceKind,
}

impl DeviceInfo {
    /// Whether this is an external USB DAC.
    pub fn is_usb(&self) -> bool {
        self.kind == DeviceKind::Usb
    }
}

/// Classify a card as USB / internal from its driver + long name (USB cards
/// carry the bus in their longname, e.g. "... at usb-0000:00:14.0-1") or use the
/// `USB-Audio` driver).
fn classify(driver: &str, longname: &str) -> DeviceKind {
    let hay = format!("{driver} {longname}").to_ascii_lowercase();
    if hay.contains("usb") {
        DeviceKind::Usb
    } else {
        DeviceKind::Internal
    }
}

/// List bit-perfect `hw:` output devices, USB DACs first. Best-effort:
/// enumeration failures are skipped, yielding whatever was reachable (an empty
/// list if nothing is), so the caller can fall back to a configured device.
pub fn list_devices() -> Vec<DeviceInfo> {
    let mut out = Vec::new();

    for card in CardIter::new().flatten() {
        let idx = card.get_index();
        let ctl = match Ctl::from_card(&card, false) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Identify the card: `id` is the `CARD=` token, `name` is human-readable.
        let info = ctl.card_info().ok();
        let card_id = info
            .as_ref()
            .and_then(|i| i.get_id().ok())
            .map(str::to_string)
            .unwrap_or_else(|| idx.to_string());
        let card_name = info
            .as_ref()
            .and_then(|i| i.get_name().ok())
            .map(str::to_string)
            .unwrap_or_else(|| card_id.clone());
        let driver = info
            .as_ref()
            .and_then(|i| i.get_driver().ok())
            .map(str::to_string)
            .unwrap_or_default();
        let longname = info
            .as_ref()
            .and_then(|i| i.get_longname().ok())
            .map(str::to_string)
            .unwrap_or_default();
        let kind = classify(&driver, &longname);

        // Enumerate this card's playback PCM devices.
        for dev in DeviceIter::new(&ctl) {
            let dev = dev as u32;
            // Keep only devices that actually support playback.
            let dev_name = match ctl.pcm_info(dev, 0, Direction::Playback) {
                Ok(pinfo) => pinfo.get_name().map(str::to_string).unwrap_or_default(),
                Err(_) => continue,
            };
            let id = format!("hw:CARD={card_id},DEV={dev}");
            let description = if dev_name.is_empty() {
                card_name.clone()
            } else {
                format!("{card_name} — {dev_name}")
            };
            out.push(DeviceInfo {
                id,
                name: card_name.clone(),
                description,
                kind,
            });
        }
    }

    // USB DACs first, then internal, then other; stable within a class.
    out.sort_by_key(|d| match d.kind {
        DeviceKind::Usb => 0,
        DeviceKind::Internal => 1,
        DeviceKind::Other => 2,
    });
    out
}

/// Pick a sensible default output: a connected USB DAC if present, else the
/// first discovered device. Returns `None` when nothing is enumerable (the
/// caller should fall back to a configured/`hw:0,0` default).
pub fn auto_pick() -> Option<DeviceInfo> {
    let devices = list_devices();
    devices
        .iter()
        .find(|d| d.is_usb())
        .cloned()
        .or_else(|| devices.into_iter().next())
}

/// Whether a USB DAC is currently connected — any enumerated bit-perfect output
/// classified as [`DeviceKind::Usb`]. Used by the GTK shell's standby logic to
/// decide when the player is truly idle (no external DAC, so nobody can be
/// listening). Best-effort, reusing the same classification as [`list_devices`].
pub fn has_usb_dac() -> bool {
    list_devices().iter().any(DeviceInfo::is_usb)
}
