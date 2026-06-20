project('phosh-plugin-hifi-usb', 'c',
  version: '0.1.0',
  license: 'GPL-3.0-or-later',
  meson_version: '>= 0.60.0',
  default_options: ['warning_level=1', 'c_std=gnu11'],
)

gnome = import('gnome')

name = 'hifi-usb'

# Phosh's plugin SDK. As with the hifi-sync tile (see its meson.build), pmOS/Alpine's
# 'phosh-plugins' pkg-config ships ONLY the plugin entry-point header + the *_plugins_dir
# directory variables — not the libphosh widget headers and no linkable libphosh — so the
# three widget headers (quick-setting.h / status-icon.h / status-page.h) are vendored under
# phosh-vendor/ and resolve from the running phosh at dlopen time. Phosh is GTK3 + libhandy.
phosh_plugins_dep = dependency('phosh-plugins')
gtk_dep   = dependency('gtk+-3.0')
handy_dep = dependency('libhandy-1')
gio_dep   = dependency('gio-2.0')
giou_dep  = dependency('gio-unix-2.0')

# Where phosh dlopen()s quick-setting plugins (= /usr/lib/phosh/plugins).
plugins_dir = phosh_plugins_dep.get_variable(
  pkgconfig: 'quick_setting_plugins_dir',
  default_value: get_option('libdir') / 'phosh' / 'plugins',
)

# Vendored phosh widget headers (quick-setting.h includes status-icon.h/status-page.h
# by "" so they must be on the quote include path).
phosh_vendor_inc = include_directories('phosh-vendor')

deps = [phosh_plugins_dep, gtk_dep, handy_dep, gio_dep, giou_dep]

resources = gnome.compile_resources(
  'phosh-plugin-hifi-usb-resources',
  'phosh-plugin-hifi-usb.gresources.xml',
  c_name: 'phosh_plugin_hifi_usb',
)

sources = files(
  'phosh-plugin-hifi-usb.c',
  'hifi-usb.c',
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
