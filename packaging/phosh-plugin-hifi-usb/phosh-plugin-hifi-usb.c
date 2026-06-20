/*
 * Copyright (C) 2026 Anton
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * GModule entry point: registers the "USB mode" quick-setting at Phosh's
 * quick-setting-widget extension point. Mirrors Phosh's own plugin boilerplate
 * (plugins/simple-custom-quick-setting); PLUGIN_NAME is -D'd by meson.
 */

#include "phosh-plugin.h"
#include "hifi-usb.h"


char **g_io_phosh_plugin_hifi_usb_query (void);


void
g_io_module_load (GIOModule *module)
{
  g_type_module_use (G_TYPE_MODULE (module));

  g_io_extension_point_implement (PHOSH_PLUGIN_EXTENSION_POINT_QUICK_SETTING_WIDGET,
                                  PHOSH_TYPE_HIFI_USB_QUICK_SETTING,
                                  PLUGIN_NAME,
                                  10);
}


void
g_io_module_unload (GIOModule *module)
{
}


char **
g_io_phosh_plugin_hifi_usb_query (void)
{
  char *extension_points[] = {PHOSH_PLUGIN_EXTENSION_POINT_QUICK_SETTING_WIDGET, NULL};

  return g_strdupv (extension_points);
}
