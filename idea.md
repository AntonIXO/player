Build it as an adaptive Linux player first, then as a phone player. For Poco F1 on postmarketOS/Phosh, Rust + GTK4/libadwaita is a sensible direction because Phosh is built around adaptive GNOME apps on Wayland, and the Phosh ecosystem is actively moving toward GTK4/libadwaita.[1][2][3]

## Stack

You need three layers: an adaptive UI, a playback/library core, and a deployment path for aarch64 postmarketOS. On the UI side, use gtk4-rs plus libadwaita; the Rust GTK docs explicitly recommend the `adw` crate and note that `gtk4` and `libadwaita` crate versions should stay in sync.[3]
Design the app for both a small touch screen and a docked keyboard/monitor setup, because Phosh explicitly targets mobile devices but also documents use with external display and input setups.[1]

## PC testing

Do not start with cross-compiling; that is the slow path. The fastest dev loop is native x86_64 Linux on your PC, then running Phosh either nested for development or as a real session via `phosh-session` or a display manager when you want shell-accurate behavior.[4][5][1]
If you want a cleaner approximation of the target userspace before touching the phone, postmarketOS can also run as a x86_64 QEMU VM.[6]

## Poco F1 reality

Your target hardware is viable: the postmarketOS Poco F1 page lists it as aarch64 with a 1080x2246 display, working touchscreen, working 3D acceleration, and working audio playback. But it is not a polished appliance: the same page marks Wi‑Fi, calls, SMS, battery, GPS, and camera support as mixed or partial in places, and it documents audio-stack instability over time, so real device audio testing should happen early, not at the end.[7]
Also, if you have not installed postmarketOS yet, you must detect whether your panel is Tianma or EBBG before `pmbootstrap init`, because the kernel variant depends on that choice.[7]

## Pipeline

1. Prototype the app on PC: build a minimal adaptive GTK4/libadwaita shell, run it under nested Phosh, and use GTK Inspector plus Phosh’s DBus mock and screenshot tooling to catch layout and shell-integration regressions fast.[1]
2. Validate in postmarketOS: either boot postmarketOS in QEMU amd64 first, or test on the Poco F1 from SD card with `pmbootstrap install --sdcard=...` and `pmbootstrap flasher boot`, which the device page documents as a way to test without disturbing the installed system.[6][7]
3. Only then solve ARM delivery: cross-compiling gtk-rs apps for ARM is possible, but in practice it usually needs a target sysroot, `PKG_CONFIG_SYSROOT_DIR`/`PKG_CONFIG_LIBDIR`, and linker configuration, and many people isolate that setup in Docker or similar containerized build environments.[8][9]

## First week

Week 1 should produce a boring but working skeleton: library view, now-playing view, queue view, touch-friendly navigation, and real playback on desktop. Week 2 should be Phosh adaptation and postmarketOS validation, because that is where the fake-desktop assumptions die.  
Want the next step to be a minimal project structure for Rust + GTK4/libadwaita, plus a concrete PC test setup for Phosh?

Citations:
[1] [Xiaomi POCO F1 (xiaomi-beryllium)/sxmo](https://wiki.postmarketos.org/wiki/Xiaomi_POCO_F1_(xiaomi-beryllium)/sxmo)  
[2] [Port Phosh to GTK4/libadwaita](https://nlnet.nl/project/Phosh-GTK4/)  
[3] [Libadwaita - GUI development with Rust and GTK 4](https://gtk-rs.org/gtk4-rs/git/book/libadwaita.html)  
[4] [phosh-session(1) — phosh — Debian experimental](https://manpages.debian.org/experimental/phosh/phosh-session.1.en.html)  
[5] [GitHub - FuriLabs/phosh](https://github.com/FuriLabs/phosh)  
[6] [QEMU amd64 (qemu-amd64)](https://wiki.postmarketos.org/wiki/QEMU_amd64_(qemu-amd64))  
[7] [Xiaomi POCO F1 (xiaomi-beryllium)](https://wiki.postmarketos.org/wiki/Xiaomi_POCO_F1_(xiaomi-beryllium))  
[8] [How to build GTK+ APP for ARM based of Linux platform](https://stackoverflow.com/questions/42851320/how-to-build-gtk-app-for-arm-based-of-linux-platform)  
[9] [Cross compiling Gtk-rs for Raspberry Pi 3 : r/rust](https://www.reddit.com/r/rust/comments/ayskny/cross_compiling_gtkrs_for_raspberry_pi_3/)  
[10] [User:FerassElHafidi/Dogfooding:Pocophone F1](https://wiki.postmarketos.org/wiki/User:FerassElHafidi/Dogfooding:Pocophone_F1)  
[11] [Postmarketos on poco f1? : r/linuxquestions](https://www.reddit.com/r/linuxquestions/comments/1rmgi5g/postmarketos_on_poco_f1/)  
[12] [The fastest mainline Linux Phone: Pocophone F1](https://www.youtube.com/watch?v=KtfTJbLiYfg)  
[13] [postmarketos-xiaomi-beryllium](https://gist.github.com/heethjain21/d71bded759451e3270a691516b99f3ab)  
[14] [Devices - postmarketOS Wiki](https://wiki.postmarketos.org/wiki/Devices)  
[15] [Libadwaita - GUI development with Rust and GTK 4](https://gtk-rs.org/gtk4-rs/stable/latest/book/libadwaita.html)  
[16] [How to cross compile a Gtk-rs program to windows](https://users.rust-lang.org/t/how-to-cross-compile-a-gtk-rs-program-to-windows/52480)  
[17] [How to Install postmarketOS on a Poco F1](https://www.youtube.com/watch?v=Rh0tA9-tVXE)  
[18] [Port Phosh to GTK4/libadwaita](https://ngi.eu/funded_solution/phosh-gtk4/)  
[19] [Getting started with Phosh development](https://world.pages.gitlab.gnome.org/Phosh/phosh/gettingstarted.html)  
[20] [Starting Phosh](https://wiki.postmarketos.org/wiki/Phosh)  
[21] [phosh-session(1) - Arch manual pages](https://man.archlinux.org/man/phosh-session.1.en)  
[22] [Session startup script for phosh - Ubuntu Manpage](https://manpages.ubuntu.com/manpages/resolute/man1/phosh-session.1.html)  
[23] [QEMU riscv64 (qemu-riscv64)](https://wiki.postmarketos.org/wiki/QEMU_riscv64_(qemu-riscv64))  
[24] [phosh-session(1) - bookworm - Debian Manpages](https://manpages.debian.org/bookworm/phosh/phosh-session.1.en.html)  
[25] [QEMU armv7 (qemu-armv7)](https://wiki.postmarketos.org/wiki/QEMU_armv7_(qemu-armv7))  
[26] [Initial setup for a GTK4 app with libadwaita in Rust using ...](https://blog.devgenius.io/initial-setup-for-a-gtk4-app-with-libadwaita-in-rust-using-vscode-b6f8c127a75e)  
[27] [phosh-session(1)](https://man.archlinux.org/man/extra/phosh/phosh-session.1.en)  
[28] [QEMU ppc64le (qemu-ppc64le)](https://wiki.postmarketos.org/wiki/QEMU_ppc64le_(qemu-ppc64le))
