project('phosh-plugin-hifi-sync', 'c',
  version: '0.1.0',
  license: 'GPL-3.0-or-later',
  meson_version: '>= 0.60.0',
  default_options: ['warning_level=1', 'c_std=gnu11'],
)

gnome = import('gnome')

name = 'hifi-sync'

# Phosh's plugin SDK. Reality on postmarketOS/Alpine (verified against phosh 0.55.0):
# the 'phosh-plugins' pkg-config file provides ONLY the plugin entry-point header
# (/usr/include/phosh/phosh-plugin.h, the extension-point name macros) plus a set of
# *_plugins_dir directory variables. It does NOT ship the libphosh widget headers
# (quick-setting.h / status-icon.h / status-page.h) and there is no linkable libphosh.
# Those types live in the running phosh process (symbols resolved at dlopen time), so
# we carry our own copies of the three headers in phosh-vendor/ (see its README).
# Phosh is GTK3 + libhandy (NOT GTK4/libadwaita); listed explicitly for a legible
# failure if a -dev package is missing.
#
# STEP 0 — verify on the device's Phosh version before building:
#     pkg-config --exists phosh-plugins
#     pkg-config --variable=quick_setting_plugins_dir phosh-plugins
phosh_plugins_dep = dependency('phosh-plugins')
gtk_dep   = dependency('gtk+-3.0')
handy_dep = dependency('libhandy-1')
gio_dep   = dependency('gio-2.0')
giou_dep  = dependency('gio-unix-2.0')

# Where phosh dlopen()s quick-setting plugins. The installed .pc exposes this as
# 'quick_setting_plugins_dir' (= /usr/lib/phosh/plugins); fall back to the same path.
plugins_dir = phosh_plugins_dep.get_variable(
  pkgconfig: 'quick_setting_plugins_dir',
  default_value: get_option('libdir') / 'phosh' / 'plugins',
)

# Vendored phosh widget headers (quick-setting.h includes status-icon.h/status-page.h
# by "" so they must be on the quote include path).
phosh_vendor_inc = include_directories('phosh-vendor')

deps = [phosh_plugins_dep, gtk_dep, handy_dep, gio_dep, giou_dep]

resources = gnome.compile_resources(
  'phosh-plugin-hifi-sync-resources',
  'phosh-plugin-hifi-sync.gresources.xml',
  c_name: 'phosh_plugin_hifi_sync',
)

sources = files(
  'phosh-plugin-hifi-sync.c',
  'hifi-sync.c',
)

shared_module('phosh-plugin-' + name,
  sources,
  resources,
  include_directories: phosh_vendor_inc,
  c_args: [
    '-DG_LOG_DOMAIN="phosh-plugin-@0@"'.format(name),
    '-DPLUGIN_NAME="@0@"'.format(name),
  ],
  dependencies: deps,
  install: true,
  install_dir: plugins_dir,
)

# Plugin metadata (.plugin) — single substitution pass; no translations needed.
pluginconf = configuration_data()
pluginconf.set('name', name)
pluginconf.set('plugins_dir', plugins_dir)

configure_file(
  input: name + '.plugin.in',
  output: name + '.plugin',
  configuration: pluginconf,
  install: true,
  install_dir: plugins_dir,
)
