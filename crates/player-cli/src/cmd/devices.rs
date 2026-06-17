//! `devices` subcommand: list bit-perfect `hw:` outputs and mark the auto-pick.

use player_core::DeviceKind;

pub fn devices() -> player_core::Result<()> {
    let devices = player_core::list_devices();
    if devices.is_empty() {
        println!("(no bit-perfect hw: output devices found)");
        return Ok(());
    }
    let pick = player_core::auto_pick().map(|d| d.id);
    for d in &devices {
        let kind = match d.kind {
            DeviceKind::Usb => "USB DAC",
            DeviceKind::Internal => "internal",
            DeviceKind::Other => "other",
        };
        let marker = if pick.as_deref() == Some(d.id.as_str()) {
            " *"
        } else {
            "  "
        };
        println!("{marker} {:<28} [{kind}]  {}", d.id, d.name);
        if !d.description.is_empty() && d.description != d.id {
            for line in d.description.lines() {
                println!("       {line}");
            }
        }
    }
    println!("\n  * = auto-pick default");
    Ok(())
}
