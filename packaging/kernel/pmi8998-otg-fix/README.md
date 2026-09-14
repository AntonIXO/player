# Poco F1: Mojo 2 disappears when the screen blanks

## Confirmed on the phone, 2026-09-14

Kernel: `7.2.3-sdm845 #2-postmarketos-qcom-sdm845`, PMI8998, Mojo 2
`245f:0815`. ScreenSaver.SetActive(true) disconnected the DAC even without a
player running, and with DWC3, xHCI and USB root hubs forced to `power/control=on`.
There was no system suspend. Cycling VBUS restored enumeration without touching
the cable. `usb-vbus-output/state` and CMD_OTG (0x1140) still said enabled after
failure; OTG status 0x1109 changed from 0x0a to 0x04 and 0x1110 from 0 to 1.
Disabling charging through charge_behaviour did not fix it and was reverted.

The vendor driver sets OTG_ENG_OTG_CFG bit 0 (ENG_BUCKBOOST_HALT1_8_MODE_BIT)
before enabling VBUS and clears it after disabling VBUS. Our regulator backport
omitted this step. Setting that bit kept the same USB device through screen
blanking. This establishes an effective fix on this phone, not an electrical
measurement of the failure waveform.

Reference: Qualcomm `_smblib_vbus_regulator_enable` / disable and its secure
register write helper:
https://android.googlesource.com/kernel/msm/+/8ac9f6ee3c57a865fa9c448873126b585f63a044/drivers/power/qcom-charger/smb-lib.c

## Installed compatibility fix

`pmi8998_otg_fix.c` applies just that bit on this machine, reads it back, and
restores its previous value on unload. It does not change current limits,
thermal protection, audio samples or USB authorization. `hifi-usb-mode.patch`
loads the module before VBUS-on and unloads it after VBUS-off for charge/sync.
The helper is called by the existing boot-time audio setup service.

Build against the **exact running kernel configuration and matching source**:

```sh
make -C "$KERNEL_BUILD" M="$PWD" ARCH=arm64 LLVM=1 modules
```

Use its Module.symvers for a normal production build. For this installed kernel,
MODVERSIONS is off; the validation build used KBUILD_MODPOST_WARN=1 because the
old build did not retain Module.symvers. Successful loading/readback and reboot
were checked on the phone. Do not suppress or force a vermagic/config mismatch.
An initial build with an older configuration caused an ftrace/Oops during module
loading (different CFI/ftrace settings). It was not installed for boot; the phone
was rebooted after installing the correctly configured build.

On the phone the module lives at
`/lib/modules/7.2.3-sdm845/extra/pmi8998_otg_fix.ko`, registered with depmod.
Original binaries and USB helper are in `/root/mojo-screen-fix/*.before`.
The compatible module must be rebuilt on a kernel update, or superseded by the
proper driver patch below.

## Proper kernel fix

`../0010-regulator-qcom-usb-vbus-pmi8998-halt-mode.patch` moves this sequence into
regulator enable/disable, scoped to PMI8998. It is also included in the audio
kernel APKBUILD. The patched driver was compile-checked; a full kernel containing
it has not been flashed. Do not use both the compatibility module and the patched
driver together: remove the module once the driver patch is deployed.

## Player crash

The installed player separately aborted after EPIPE: recovery created `io_bytes`
while a write still held another ALSA IO object. The fix drops that IO before
recovery and reacquires it afterward in all three write paths. Failed silence
priming now returns an error instead of continuing or spinning on zero writes.
A regression test causes an actual snd-aloop underrun in S16, S24_3 and S32.

## Verification

- 13 normal player-core tests passed; explicit real-XRUN regression also passed.
- Phone: three screen on/off cycles during 35 seconds of engine playback of
  digital silence; same USB address 7 throughout, RUNNING at each check,
  1,680,000 frames, zero xruns, successful playback exit.
- Both player-gtk and player-cli were built on-device and installed.
- Runtime USB power overrides were diagnostic only; reboot restores defaults.
- Reboot verified: the correct module loaded automatically at uptime 16.6s;
  audio-setup succeeded; player-gtk autostarted and Mojo was the USB default.
- ALSA loopback compared 84,000 nonzero stereo frames byte-for-byte after
  capture alignment (96,000 frames supplied): MATCH / BIT-PERFECT.
- After reboot, blanking the display (bl_power=4) kept USB address 2 and the
  player process alive, with root-hub power/control back at its default `auto`.
  The graphical library screen was captured and inspected; the screen was then
  left on with the player running.
- Transfer to master: kernel patch numbered 0010 and wired into the 7.2.3 aport;
  USB helper changes applied directly to its packaged source. On master's ALSA
  0.12.1 dependency, 18 normal core tests and the real-XRUN regression passed.
