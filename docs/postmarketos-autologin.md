# postmarketOS autologin on the player phone

## Result

Automatic login is enabled and verified on the Poco F1 player at
`root@192.168.31.125`. A real reboot opened the non-root Phosh session for
`antonix` immediately; neither the phrog greeter nor a password prompt was used.
`player-gtk` then started through the existing XDG autostart entry.

This is a graphical-login change, not a disk-unlock bypass. The installed root
filesystem is plain F2FS on `/dev/loop0p2`, `/etc/crypttab` is absent, and no LUKS
mapping backs `/`. postmarketOS only creates an encrypted root when the image is
installed with `pmbootstrap install --fde`.[^1]

## Root cause

The `hifi-player` package already generated a valid greetd configuration:

```toml
[default_session]
command = "/usr/libexec/phrog-greetd-session"
user = "greetd"

[initial_session]
command = "/usr/bin/phosh-session"
user = "antonix"
```

greetd defines `initial_session` as its one-time-per-boot autologin path.[^2]
However, the package installed its systemd drop-in as
`90-hifi-player-autologin.conf`. postmarketOS' Phosh systemd subpackage installs
an unnumbered `override.conf` that resets `ExecStart` to:

```ini
ExecStart=greetd -c /etc/phrog/greetd-config.toml
```

systemd merges differently named drop-ins in lexicographic order, regardless of
whether they live in `/usr/lib` or `/etc`; later assignments win after a list such
as `ExecStart` is reset.[^3] Because `override.conf` sorts after
`90-hifi-player-autologin.conf`, the stock phrog configuration won and displayed
the login prompt. The current upstream postmarketOS Phosh package still installs
that same unnumbered override.[^4]

## Applied fix

The live phone now has this administrator override:

```text
/etc/systemd/system/greetd.service.d/zz-hifi-player-autologin.conf
```

It sorts after `override.conf`, runs
`/usr/libexec/hifi-player-greetd-config`, and starts greetd with the generated
configuration under `/run/hifi-player-greetd/config.toml`.

The repository package was fixed in parallel: its installed filename changed
from `90-hifi-player-autologin.conf` to `zz-hifi-player-autologin.conf`. This keeps
future APK builds from reintroducing the ordering bug.

## Reboot verification

The reboot changed the kernel boot ID from
`ef3313c3-6691-4a94-bc28-a2b22f62ab0a` to
`61b8e0d2-ccfb-4935-8be4-a37593722292`. In the new boot:

- `greetd.service` is active and uses
  `/run/hifi-player-greetd/config.toml`;
- its first and only session-open record is
  `session opened for user antonix(uid=10000)`;
- `phosh`, `phoc`, and `player-gtk` run as `antonix`;
- no `phrog` process or `greetd` greeter-user session exists;
- `org.gnome.desktop.screensaver lock-enabled` and the live
  `org.gnome.ScreenSaver.GetActive` value are both `false`.

## Security and rollback

Anyone with physical access can now reach the player UI and the logged-in
`antonix` desktop after powering on the phone. The account password and PAM remain
available for privilege elevation; autologin does not log in as root.

To restore the normal password prompt:

```sh
sed -i 's/^HIFI_PLAYER_AUTOLOGIN=.*/HIFI_PLAYER_AUTOLOGIN=0/' \
  /etc/default/hifi-player
reboot
```

The generated config will then retain only the phrog fallback session.

## Sources

[^1]: postmarketOS, [pmbootstrap usage: `install --fde`](https://docs.postmarketos.org/pmbootstrap/main/usage.html#pmbootstrap-install).
[^2]: greetd, [`initial_session` configuration](https://man.archlinux.org/man/greetd.5.en#initial_session).
[^3]: systemd, [unit drop-in ordering and precedence](https://man.archlinux.org/man/systemd.unit.5.en#DESCRIPTION).
[^4]: postmarketOS pmaports, [`postmarketos-ui-phosh` APKBUILD at the inspected upstream revision](https://gitlab.postmarketos.org/postmarketOS/pmaports/-/blob/1ea42816e99975debc93e88fae1849de73308a41/main/postmarketos-ui-phosh/APKBUILD#L61-70) and [`greetd-phrog.conf`](https://gitlab.postmarketos.org/postmarketOS/pmaports/-/blob/1ea42816e99975debc93e88fae1849de73308a41/main/postmarketos-ui-phosh/greetd-phrog.conf).
