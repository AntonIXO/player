/*
 * Copyright (C) 2026 Anton
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * "USB mode" quick setting for Phosh. The USB-C port is one of three things at a
 * time on this dedicated player: powering the USB DAC (Audio), charging the phone
 * (Charging), or a wired data link for fast music sync (Sync). The tile shows the
 * current mode; tapping it opens a status page with three buttons to switch.
 *
 * All the real (privileged) work lives in /usr/bin/hifi-usb-mode, driven here via
 * the templated hifi-usb-mode@<mode>.service unit (a scoped polkit rule lets the
 * session user start it). This widget only spawns that asynchronously and reflects
 * the status it reads back, so the UI thread never blocks on the role/VBUS/gadget
 * transition.
 */

#include "hifi-usb.h"

#include <glib/gi18n.h>
#include <gio/gio.h>

#define HIFI_USB_MODE "/usr/bin/hifi-usb-mode"

struct _PhoshHifiUsbQuickSetting {
  PhoshQuickSetting  parent;

  PhoshStatusIcon   *info;   /* tile icon + small status text  (template child) */
  GtkLabel          *label;  /* detail line in the status page (template child) */
};

G_DEFINE_TYPE (PhoshHifiUsbQuickSetting, phosh_hifi_usb_quick_setting, PHOSH_TYPE_QUICK_SETTING);


static const char *
mode_icon (const char *mode)
{
  if (g_strcmp0 (mode, "audio") == 0)  return "audio-headphones-symbolic";
  if (g_strcmp0 (mode, "charge") == 0) return "battery-good-symbolic";
  if (g_strcmp0 (mode, "sync") == 0)   return "folder-download-symbolic";
  return "content-loading-symbolic";
}


static const char *
mode_title (const char *mode)
{
  if (g_strcmp0 (mode, "audio") == 0)  return _("USB: Audio");
  if (g_strcmp0 (mode, "charge") == 0) return _("USB: Charging");
  if (g_strcmp0 (mode, "sync") == 0)   return _("USB: Sync");
  return _("USB mode");
}


static void
apply_state (PhoshHifiUsbQuickSetting *self, const char *mode, const char *detail)
{
  /* The tile reads "active" (highlighted) only when the port is powering the DAC. */
  phosh_quick_setting_set_active (PHOSH_QUICK_SETTING (self), g_strcmp0 (mode, "audio") == 0);
  phosh_status_icon_set_icon_name (self->info, mode_icon (mode));
  phosh_status_icon_set_info (self->info, mode_title (mode));
  if (self->label != NULL && detail != NULL && detail[0] != '\0')
    gtk_label_set_label (self->label, detail);
}


/* Parse one `hifi-usb-mode status` line: "<audio|charge|sync> <detail…>" and sync
 * the tile to the device's real state. Owns a ref taken before the spawn. */
static void
on_status_done (GObject *source, GAsyncResult *res, gpointer user_data)
{
  PhoshHifiUsbQuickSetting *self = user_data;
  g_autoptr (GError) error = NULL;
  g_autofree char *out = NULL;

  if (!g_subprocess_communicate_utf8_finish (G_SUBPROCESS (source), res, &out, NULL, &error)) {
    g_warning ("hifi-usb-mode status failed: %s", error->message);
    g_object_unref (self);
    return;
  }

  g_auto (GStrv) f = g_strsplit (g_strchomp (out != NULL ? out : ""), " ", 2);
  const char *mode   = (g_strv_length (f) >= 1 && f[0][0] != '\0') ? f[0] : "unknown";
  const char *detail = (g_strv_length (f) >= 2) ? f[1] : "";

  apply_state (self, mode, detail);
  g_object_unref (self);
}


static void
refresh_status (PhoshHifiUsbQuickSetting *self)
{
  g_autoptr (GError) error = NULL;
  GSubprocess *proc = g_subprocess_new (G_SUBPROCESS_FLAGS_STDOUT_PIPE, &error,
                                        HIFI_USB_MODE, "status", NULL);
  if (proc == NULL) {
    g_warning ("could not run hifi-usb-mode status: %s", error->message);
    return;
  }
  g_subprocess_communicate_utf8_async (proc, NULL, NULL, on_status_done, g_object_ref (self));
  g_object_unref (proc);
}


/* When the switch finishes, re-read the true status so the tile reflects it (and
 * reverts if the mode change was refused, e.g. a stream was still playing). */
static void
on_action_done (GObject *source, GAsyncResult *res, gpointer user_data)
{
  PhoshHifiUsbQuickSetting *self = user_data;

  g_subprocess_wait_finish (G_SUBPROCESS (source), res, NULL);
  refresh_status (self);
  g_object_unref (self);
}


/* Switch to one of the three modes by starting hifi-usb-mode@<mode>.service via
 * systemctl (allowed for the active session user by 10-hifi-usb-mode.rules). */
static void
switch_mode (PhoshHifiUsbQuickSetting *self, const char *mode)
{
  /* Optimistic feedback; the real state is confirmed in on_action_done. */
  apply_state (self, mode, _("Switching…"));
  phosh_quick_setting_set_showing_status (PHOSH_QUICK_SETTING (self), FALSE);

  g_autofree char *unit = g_strdup_printf ("hifi-usb-mode@%s.service", mode);
  g_autoptr (GError) error = NULL;
  GSubprocess *proc = g_subprocess_new (G_SUBPROCESS_FLAGS_STDOUT_SILENCE, &error,
                                        "systemctl", "start", unit, NULL);
  if (proc == NULL) {
    g_warning ("could not start %s: %s", unit, error->message);
    refresh_status (self);   /* resync the tile to reality */
    return;
  }
  g_subprocess_wait_async (proc, NULL, on_action_done, g_object_ref (self));
  g_object_unref (proc);
}


static void on_mode_audio  (PhoshHifiUsbQuickSetting *self) { switch_mode (self, "audio"); }
static void on_mode_charge (PhoshHifiUsbQuickSetting *self) { switch_mode (self, "charge"); }
static void on_mode_sync   (PhoshHifiUsbQuickSetting *self) { switch_mode (self, "sync"); }


/* Tapping the tile opens the status page with the three mode buttons. */
static void
on_clicked (PhoshHifiUsbQuickSetting *self)
{
  phosh_quick_setting_set_showing_status (PHOSH_QUICK_SETTING (self), TRUE);
}


static void
phosh_hifi_usb_quick_setting_class_init (PhoshHifiUsbQuickSettingClass *klass)
{
  GtkWidgetClass *widget_class = GTK_WIDGET_CLASS (klass);

  gtk_widget_class_set_template_from_resource (widget_class,
                                               "/org/player/phosh/plugins/hifi-usb/qs.ui");

  gtk_widget_class_bind_template_child (widget_class, PhoshHifiUsbQuickSetting, info);
  gtk_widget_class_bind_template_child (widget_class, PhoshHifiUsbQuickSetting, label);

  gtk_widget_class_bind_template_callback (widget_class, on_clicked);
  gtk_widget_class_bind_template_callback (widget_class, on_mode_audio);
  gtk_widget_class_bind_template_callback (widget_class, on_mode_charge);
  gtk_widget_class_bind_template_callback (widget_class, on_mode_sync);
}


static void
phosh_hifi_usb_quick_setting_init (PhoshHifiUsbQuickSetting *self)
{
  gtk_widget_init_template (GTK_WIDGET (self));
  phosh_quick_setting_set_can_show_status (PHOSH_QUICK_SETTING (self), TRUE);
  refresh_status (self);   /* show the true state at construction time */
}
