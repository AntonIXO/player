# CURTAIN.md — building a Phosh "curtain" (quick-settings) button

How to add a tile to the **Phosh system curtain** (the pull-down quick-settings panel),
the hard way it actually works on postmarketOS — not the way the upstream tutorial
implies. Written from building the **"Music sync"** tile in
`packaging/phosh-plugin-hifi-sync/` (see also the [[project-music-sync-sftp]] feature and
`CLAUDE.md` › "GTK shell"). Everything here was verified against **Phosh 0.55.0** in the
aarch64 pmbootstrap chroot on the Poco F1.

> TL;DR: A curtain button is a **`PhoshQuickSetting` GObject** compiled as a **GModule**
> that phosh `dlopen()`s. It's **GTK3 + libhandy** (not GTK4/libadwaita). pmOS `phosh-dev`
> does **not** ship the widget headers, so you **vendor** them. The widget must genuinely
> subclass `PhoshQuickSetting` (a plain `GtkWidget` crashes/destabilises the curtain). Keep
> the widget thin — shell out to a script **asynchronously**.

---

## 1. What a curtain button is

Phosh exposes extension points; custom quick-settings tiles implement
`PHOSH_PLUGIN_EXTENSION_POINT_QUICK_SETTING_WIDGET` (`"phosh-quick-setting-widget"`). A
plugin is a shared module loaded into the **phosh process** and enabled via GSettings:

```sh
gsettings set sm.puri.phosh.plugins quick-settings "['hifi-sync']"
```

The list contains **custom plugin tiles only** — the built-in Wi-Fi/BT/torch toggles are
separate and unaffected. The string is the plugin **Id** (its `.plugin` `Id=` / meson
`name`), *not* the `.so` name.

The canonical reference is upstream's `plugins/simple-custom-quick-setting/` — read it at
the **tag matching your device's phosh version** (the third-party plugin API is not
guaranteed stable):
`https://gitlab.gnome.org/World/Phosh/phosh/-/tree/v0.55.0/plugins/simple-custom-quick-setting`

---

## 2. The postmarketOS SDK reality (the big gotcha)

The upstream tutorial assumes you build **in the phosh source tree**, where the widget
headers are on the include path. Out-of-tree on pmOS/Alpine they are **not installed**:

`phosh-dev` ships only

- `/usr/include/phosh/phosh-plugin.h` — the extension-point **name macros only**, and
- `/usr/include/phosh/phosh-settings-enums.h`, and
- `phosh-plugins.pc` — **directory variables**, with `Cflags: -I/usr/include/phosh`.

It does **not** ship `quick-setting.h`, `status-icon.h`, `status-page.h`, and there is **no
linkable `libphosh`**. The widget *types* exist only inside the running phosh process,
which provides the symbols at `dlopen` time. So a plugin compiled against just what pmOS
ships fails with:

```
fatal error: quick-setting.h: No such file or directory
```

**Fix: vendor the three headers** (see §5). Don't chase a `-dev` package that doesn't exist.

`phosh-plugins.pc` exposes these variables (read them, don't hardcode):

```
quick_setting_plugins_dir = /usr/lib/phosh/plugins     # <-- install the .so + .plugin here
quick_setting_prefs_dir   = /usr/lib/phosh/plugins/prefs
lockscreen_plugins_dir    = /usr/lib/phosh/plugins
status_icons_plugins_dir  = /usr/lib/phosh/plugins
```

> ⚠️ The variable is **`quick_setting_plugins_dir`**, not `plugins_dir` (the in-tree build
> defines `plugins_dir` itself; the installed `.pc` does not). Asking for `plugins_dir`
> "works" only by falling through to a default — don't rely on that.

---

## 3. Anatomy of a plugin

Six files (mirroring `packaging/phosh-plugin-hifi-sync/`):

| File | Role |
|---|---|
| `phosh-plugin-<name>.c` | GModule entry point — registers the GType at the extension point. |
| `<name>.h` / `<name>.c` | The `PhoshQuickSetting` subclass (the widget + behaviour). |
| `qs.ui` | GTK3 `GtkBuilder` template for the tile + its status page. |
| `<name>.gresources.xml` | Bundles `qs.ui` into the module. |
| `<name>.plugin.in` | Metadata phosh reads to map Id → `.so` + type. |
| `meson.build` | Build: deps, vendored-header include dir, install dirs, `.plugin` gen. |
| `phosh-vendor/*.h` | **Vendored** phosh widget headers (see §5). |

### Entry point (`phosh-plugin-<name>.c`)

```c
#include "phosh-plugin.h"      /* shipped by phosh-dev: the extension-point macros */
#include "hifi-sync.h"

char **g_io_phosh_plugin_hifi_sync_query (void);   /* name: dashes -> underscores */

void g_io_module_load (GIOModule *module)
{
  g_type_module_use (G_TYPE_MODULE (module));
  g_io_extension_point_implement (PHOSH_PLUGIN_EXTENSION_POINT_QUICK_SETTING_WIDGET,
                                  PHOSH_TYPE_HIFI_SYNC_QUICK_SETTING,
                                  PLUGIN_NAME,   /* -D'd by meson; must equal the Id */
                                  10);           /* priority */
}
void g_io_module_unload (GIOModule *module) {}
char **g_io_phosh_plugin_hifi_sync_query (void)
{
  char *eps[] = { PHOSH_PLUGIN_EXTENSION_POINT_QUICK_SETTING_WIDGET, NULL };
  return g_strdupv (eps);
}
```

`PLUGIN_NAME` and `G_LOG_DOMAIN` are passed via meson `c_args` (`-DPLUGIN_NAME="..."`).
The `query` function name is `g_io_phosh_plugin_<name>_query` with **dashes replaced by
underscores** — get this wrong and the module won't register.

### `.plugin.in` (it's a `[Plugin]` keyfile, *not* `[Desktop Entry]`)

```ini
[Plugin]
Id=@name@
Name=Music Sync
Types=quick-setting;
Comment=Share storage over Wi-Fi to drag music onto the phone
Plugin=@plugins_dir@/libphosh-plugin-@name@.so
NoDisplay=true
```

`@plugins_dir@`/`@name@` are substituted at build time. The installed file is
`<name>.plugin` in `quick_setting_plugins_dir`. Upstream produces it with
`i18n.merge_file(type:'desktop')`; if you have no translations, a plain
`configure_file()` is fine.

---

## 4. The `PhoshQuickSetting` contract (why a plain widget won't do)

The loader's extension point only requires `GTK_TYPE_WIDGET`
(`g_io_extension_point_set_required_type(ep, GTK_TYPE_WIDGET)`), so a bare `GtkWidget`
*loads*. But the quick-settings box then does:

```c
phosh_quick_settings_box_add (self->box, PHOSH_QUICK_SETTING (widget));
```

It **casts to and manages the widget as a `PhoshQuickSetting`** (active state, long-press →
status page, styling). A non-`PhoshQuickSetting` throws GLib `CRITICAL`s and can destabilise
the whole curtain. **So the tile must genuinely subclass `PhoshQuickSetting`** — which is
exactly why the header has to be available at build time (§5).

`PhoshQuickSetting` is `G_DECLARE_DERIVABLE_TYPE(..., GtkBox)`. Subclass it with
`G_DECLARE_FINAL_TYPE(..., PhoshQuickSetting)` + `G_DEFINE_TYPE`, embed the parent by value,
and add template children:

```c
struct _PhoshHifiSyncQuickSetting {
  PhoshQuickSetting  parent;
  PhoshStatusIcon   *info;    /* template child: tile icon + short status text */
  GtkLabel          *label;   /* template child: detail line in the status page */
};
G_DEFINE_TYPE (PhoshHifiSyncQuickSetting, phosh_hifi_sync_quick_setting, PHOSH_TYPE_QUICK_SETTING);

static void
phosh_hifi_sync_quick_setting_class_init (PhoshHifiSyncQuickSettingClass *klass)
{
  GtkWidgetClass *wc = GTK_WIDGET_CLASS (klass);
  gtk_widget_class_set_template_from_resource (wc, "/org/player/phosh/plugins/hifi-sync/qs.ui");
  gtk_widget_class_bind_template_child (wc, PhoshHifiSyncQuickSetting, info);
  gtk_widget_class_bind_template_child (wc, PhoshHifiSyncQuickSetting, label);
  gtk_widget_class_bind_template_callback (wc, on_clicked);
}
static void
phosh_hifi_sync_quick_setting_init (PhoshHifiSyncQuickSetting *self)
{
  gtk_widget_init_template (GTK_WIDGET (self));
}
```

### `qs.ui` skeleton (GTK3!)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<interface>
  <requires lib="gtk+" version="3.24"/>
  <template class="PhoshHifiSyncQuickSetting" parent="PhoshQuickSetting">
    <property name="status-icon">info</property>
    <property name="status-page">status_page</property>
    <signal name="clicked" handler="on_clicked" object="PhoshHifiSyncQuickSetting" swapped="yes"/>
  </template>
  <object class="PhoshStatusIcon" id="info">
    <property name="visible">1</property>
    <property name="pixel-size">16</property>
  </object>
  <object class="PhoshStatusPage" id="status_page">
    <property name="visible">1</property>
    <property name="title" translatable="yes">Music sync</property>
    <property name="content">placeholder</property>
  </object>
  <object class="PhoshStatusPagePlaceholder" id="placeholder">
    <property name="visible">1</property>
    <property name="icon-name">network-wireless-symbolic</property>
    <property name="extra-widget">label</property>
  </object>
  <object class="GtkLabel" id="label"><property name="visible">1</property></object>
</interface>
```

- The tile emits **`clicked`**; the **status page** opens on long-press (or via a
  long-press action). `PhoshStatusPagePlaceholder` is a real phosh type.
- The `<template class=...>` name must equal the GType name; the gresource path in the C
  `set_template_from_resource` must equal the `<gresource prefix>` + `qs.ui`.
- GTK3 props: `pixel-size` (not `icon-size`), `visible`, etc.

---

## 5. Vendoring the widget headers, and why it's ABI-safe

Copy these **byte-for-byte from the matching phosh tag** into `phosh-vendor/` and add it to
the include path (`include_directories('phosh-vendor')`):

- `quick-setting.h` (it `#include`s the other two by `""`, so all three must sit together)
- `status-icon.h`
- `status-page.h`

This is safe across phosh **0.x point releases**, not just the exact version, because:

1. **We never touch any phosh type's fields** — every call is an accessor function
   (`phosh_quick_setting_set_active`, `phosh_status_icon_set_info`, …) resolved **by name**
   from the phosh process at `dlopen`. No struct offsets are compiled in.
2. The **only** type we subclass, `PhoshQuickSetting`, is `GtkBox` + **10 reserved vfunc
   slots** — phosh keeps that class struct intentionally ABI-stable.
3. GObject takes the parent's real `class_size`/`instance_size` from the **runtime** type,
   not from our header (`G_DECLARE_DERIVABLE_TYPE` auto-generates the instance struct as
   `{ GtkBox parent_instance; }`).
4. GTK3/GtkBox/GtkBin/GObject are a frozen ABI.

**Re-vendor only on a phosh major/toolkit change** (the eventual GTK4 port, or if the QS
class struct ever changes). The package build (§6) is gated and best-effort, so a mismatch
degrades to "no tile", never a crash. Record the source tag in `phosh-vendor/README`.

---

## 6. Build & packaging

### meson essentials

```meson
phosh_plugins_dep = dependency('phosh-plugins')   # entry-point header + dir vars
gtk_dep   = dependency('gtk+-3.0')                 # GTK3, NOT gtk4
handy_dep = dependency('libhandy-1')               # libhandy, NOT libadwaita
gio_dep   = dependency('gio-2.0')

plugins_dir = phosh_plugins_dep.get_variable(
  pkgconfig: 'quick_setting_plugins_dir',
  default_value: get_option('libdir') / 'phosh' / 'plugins')

phosh_vendor_inc = include_directories('phosh-vendor')

resources = gnome.compile_resources('phosh-plugin-<name>-resources',
                                    'phosh-plugin-<name>.gresources.xml',
                                    c_name: 'phosh_plugin_<name>')

shared_module('phosh-plugin-' + name,          # -> libphosh-plugin-<name>.so
  sources, resources,
  include_directories: phosh_vendor_inc,
  c_args: ['-DG_LOG_DOMAIN="phosh-plugin-@0@"'.format(name),
           '-DPLUGIN_NAME="@0@"'.format(name)],
  dependencies: [phosh_plugins_dep, gtk_dep, handy_dep, gio_dep],
  install: true, install_dir: plugins_dir)
# + configure_file()/i18n.merge_file() to produce <name>.plugin into plugins_dir
```

The module name prefix **must** be `phosh-plugin-` so the `.so` becomes
`libphosh-plugin-<name>.so` (what the `.plugin` `Plugin=` line points at).

### APKBUILD (out-of-tree, best-effort, version-gated)

- `makedepends="… meson phosh-dev gtk+3.0-dev libhandy1-dev glib-dev"`
- Build only when the SDK is present **and** phosh is in the vendored ABI band; skip
  cleanly otherwise (the headless share/CLI still works):

```sh
if command -v meson >/dev/null 2>&1 && pkg-config --exists phosh-plugins 2>/dev/null; then
  if pkg-config --atleast-version=0.40 phosh-plugins \
     && ! pkg-config --atleast-version=1.0 phosh-plugins; then
     meson setup ... && meson compile -C ...   # else warn + skip
  fi
fi
```

`package()` installs the built tree (`meson install --no-rebuild`) only if it exists.

### Enable the tile by default

Ship a gschema override and compile schemas in `post-install`:

```ini
# zz-hifi-sync.gschema.override   (zz- so it applies last; user dconf still wins)
[sm.puri.phosh.plugins]
quick-settings=['hifi-sync']
```
```sh
glib-compile-schemas /usr/share/glib-2.0/schemas >/dev/null 2>&1 || true
```

---

## 7. Widget behaviour: keep it thin and async

The tile runs in **phosh's main loop** — never block it. Drive real work via a script and
`GSubprocess` **asynchronously**; reflect state with `phosh_quick_setting_set_active()` and
the status icon/page. Pattern used by the Music-sync tile:

- **on click**: read current active → optimistic `apply_state(!active, "…")` for instant
  feedback → spawn `hifi-share start|stop` with `g_subprocess_wait_async`.
- **on completion**: re-read true state by spawning `hifi-share status`
  (`g_subprocess_communicate_utf8_async`) and parsing one line.
- **ref discipline**: pass `g_object_ref(self)` as the async `user_data`, `g_object_unref`
  in the callback. Don't capture `self` without a ref — the curtain can dispose the tile.
- **on construction** (`init`): kick a `status` read so the tile shows the true state.

Backing all logic in an external script (here `/usr/bin/hifi-share`) keeps the C tiny,
makes the behaviour testable without phosh, and lets a package upgrade fix logic without
touching the compiled module.

---

## 8. phosh widget API quick reference (v0.55.0)

`PhoshQuickSetting` (`G_DECLARE_DERIVABLE_TYPE`, parent `GtkBox`):

```
phosh_quick_setting_set_active / get_active            (gboolean)        — the toggle state
phosh_quick_setting_set_status_icon / get_status_icon  (PhoshStatusIcon*)
phosh_quick_setting_set_status_page / get_status_page  (PhoshStatusPage*)
phosh_quick_setting_set_can_show_status / get_…        (gboolean)
phosh_quick_setting_set_showing_status / get_…         (gboolean)
phosh_quick_setting_set_long_press_action_name / …     (const char*)     — opens settings
phosh_quick_setting_set_long_press_action_target / …   (const char*)
```

`PhoshStatusIcon` (parent `GtkBin`): `set_icon_name`, `set_info`, `set_pixel_size`,
`set_extra_widget`, `set_priority` (+ deprecated `set_icon_size`).

`PhoshStatusPage` (parent `GtkBin`): `set_title`, `set_header`, `set_content`, `set_footer`.

---

## 9. Verify / debug on device

```sh
# What phosh advertises and where it loads plugins from:
pkg-config --exists phosh-plugins && pkg-config --modversion phosh-plugins
pkg-config --variable=quick_setting_plugins_dir phosh-plugins
ls -l /usr/include/phosh/                       # confirm only phosh-plugin.h is there

# After install, both must exist:
ls -l /usr/lib/phosh/plugins/libphosh-plugin-hifi-sync.so
ls -l /usr/lib/phosh/plugins/hifi-sync.plugin

# Enable + check:
gsettings get sm.puri.phosh.plugins quick-settings
# Re-open the curtain (or restart phosh) to reload plugins.

# If the tile is missing, watch the shell log:
journalctl --user -u phosh -f          # or: journalctl -f | grep -i phosh
#   "Custom quick setting 'NAME' not found"  -> .plugin/.so missing, or Id != gsettings entry
#   GLib-GObject CRITICAL casts            -> widget isn't a real PhoshQuickSetting
```

---

## 10. Pitfalls checklist

- [ ] It's **GTK3 + libhandy**, not GTK4/libadwaita. `qs.ui` declares `gtk+ 3.24`.
- [ ] The widget **subclasses `PhoshQuickSetting`** — never a bare `GtkWidget`.
- [ ] **Vendor** `quick-setting.h`/`status-icon.h`/`status-page.h`; don't expect `phosh-dev`
      to ship them. Keep all three together (they `#include ""` each other).
- [ ] Install to **`quick_setting_plugins_dir`** from the `.pc`; module name prefix
      `phosh-plugin-` so the `.so` matches the `.plugin` `Plugin=` line.
- [ ] `query` fn = `g_io_phosh_plugin_<name>_query` (dashes → underscores). `PLUGIN_NAME`
      must equal the `.plugin` `Id` and the gsettings entry.
- [ ] gresource path in C == `<gresource prefix>` + `qs.ui`; `<template class>` == GType name.
- [ ] Keep the widget **non-blocking** — shell out async.
- [ ] gschema override needs **`glib-compile-schemas`** to take effect.
- [ ] Package build is **best-effort + version-gated** so a missing/incompatible SDK never
      breaks the package.

---

## 11. References

- Worked example in this repo: `packaging/phosh-plugin-hifi-sync/` (the "Music sync" tile),
  wired up in `packaging/aports/hifi-player/APKBUILD` + `…/hifi-player.post-install`.
- Upstream example: phosh `plugins/simple-custom-quick-setting/` (read it at your device's
  tag).
- Upstream guide: <https://phosh.mobi/posts/custom-plugins-dev/> (assumes in-tree; adjust
  per §2).
