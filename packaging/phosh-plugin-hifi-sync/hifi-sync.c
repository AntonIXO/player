/*
 * Copyright (C) 2026 Anton
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * "Music sync" quick setting for Phosh. One tap on the system-curtain tile brings
 * Wi-Fi up and exposes the phone's storage as a writable SFTP share, so music can
 * be dragged onto it from GNOME Nautilus over Wi-Fi; tapping again tears the share
 * down, re-indexes whatever was pushed, and turns Wi-Fi off again.
 *
 * All the real work lives in /usr/bin/hifi-share (Wi-Fi + sshd/avahi + scan); this
 * widget only drives that script asynchronously and reflects its status, so the
 * UI thread never blocks on the (multi-second) Wi-Fi connect.
 */

#include "hifi-sync.h"

#include <glib/gi18n.h>
#include <gio/gio.h>

#define HIFI_SHARE "/usr/bin/hifi-share"

struct _PhoshHifiSyncQuickSetting {
  PhoshQuickSetting  parent;

  PhoshStatusIcon   *info;   /* tile icon + small status text  (template child) */
  GtkLabel          *label;  /* detail line in the status page (template child) */
};

G_DEFINE_TYPE (PhoshHifiSyncQuickSetting, phosh_hifi_sync_quick_setting, PHOSH_TYPE_QUICK_SETTING);


static void
apply_state (PhoshHifiSyncQuickSetting *self, gboolean active, const char *detail)
{
  phosh_quick_setting_set_active (PHOSH_QUICK_SETTING (self), active);
  phosh_status_icon_set_icon_name (self->info,
                                   active ? "network-wireless-symbolic"
                                          : "network-wireless-offline-symbolic");
  phosh_status_icon_set_info (self->info, _("Music sync"));
  if (self->label != NULL && detail != NULL)
    gtk_label_set_label (self->label, detail);
}


/* Parse one `hifi-share status` line: "<active|inactive> <ssid> <ip> <url>" and
 * sync the tile to the device's real state. Owns a ref taken before the spawn. */
static void
on_status_done (GObject *source, GAsyncResult *res, gpointer user_data)
{
  PhoshHifiSyncQuickSetting *self = user_data;
  g_autoptr (GError) error = NULL;
  g_autofree char *out = NULL;

  if (!g_subprocess_communicate_utf8_finish (G_SUBPROCESS (source), res, &out, NULL, &error)) {
    g_warning ("hifi-share status failed: %s", error->message);
    g_object_unref (self);
    return;
  }

  g_auto (GStrv) f = g_strsplit (g_strchomp (out != NULL ? out : ""), " ", 4);
  guint n = g_strv_length (f);
  gboolean active = (n >= 1 && g_strcmp0 (f[0], "active") == 0);
  const char *url = (n >= 4) ? f[3] : "";

  g_autofree char *detail = active
    ? g_strdup_printf (_("Sharing over Wi-Fi.\nDrop music from your computer at:\n%s"), url)
    : g_strdup (_("Tap to share over Wi-Fi, then drag music here from Nautilus."));

  apply_state (self, active, detail);
  g_object_unref (self);
}


static void
refresh_status (PhoshHifiSyncQuickSetting *self)
{
  g_autoptr (GError) error = NULL;
  GSubprocess *proc = g_subprocess_new (G_SUBPROCESS_FLAGS_STDOUT_PIPE, &error,
                                        HIFI_SHARE, "status", NULL);
  if (proc == NULL) {
    g_warning ("could not run hifi-share status: %s", error->message);
    return;
  }
  g_subprocess_communicate_utf8_async (proc, NULL, NULL, on_status_done, g_object_ref (self));
  g_object_unref (proc);
}


/* When start/stop returns, re-read the true status so the tile reflects it. */
static void
on_action_done (GObject *source, GAsyncResult *res, gpointer user_data)
{
  PhoshHifiSyncQuickSetting *self = user_data;

  g_subprocess_wait_finish (G_SUBPROCESS (source), res, NULL);
  refresh_status (self);
  g_object_unref (self);
}


static void
on_clicked (PhoshHifiSyncQuickSetting *self)
{
  gboolean active = phosh_quick_setting_get_active (PHOSH_QUICK_SETTING (self));
  const char *verb = active ? "stop" : "start";

  /* Optimistic feedback while hifi-share runs (the Wi-Fi connect takes seconds);
   * the real state is confirmed in on_action_done -> refresh_status. */
  apply_state (self, !active,
               active ? _("Turning Wi-Fi off…") : _("Connecting Wi-Fi…"));

  g_autoptr (GError) error = NULL;
  GSubprocess *proc = g_subprocess_new (G_SUBPROCESS_FLAGS_STDOUT_SILENCE, &error,
                                        HIFI_SHARE, verb, NULL);
  if (proc == NULL) {
    g_warning ("could not run hifi-share %s: %s", verb, error->message);
    refresh_status (self);   /* resync the tile to reality */
    return;
  }
  g_subprocess_wait_async (proc, NULL, on_action_done, g_object_ref (self));
  g_object_unref (proc);
}


static void
phosh_hifi_sync_quick_setting_class_init (PhoshHifiSyncQuickSettingClass *klass)
{
  GtkWidgetClass *widget_class = GTK_WIDGET_CLASS (klass);

  gtk_widget_class_set_template_from_resource (widget_class,
                                               "/org/player/phosh/plugins/hifi-sync/qs.ui");

  gtk_widget_class_bind_template_child (widget_class, PhoshHifiSyncQuickSetting, info);
  gtk_widget_class_bind_template_child (widget_class, PhoshHifiSyncQuickSetting, label);

  gtk_widget_class_bind_template_callback (widget_class, on_clicked);
}


static void
phosh_hifi_sync_quick_setting_init (PhoshHifiSyncQuickSetting *self)
{
  gtk_widget_init_template (GTK_WIDGET (self));
  refresh_status (self);   /* show the true state at construction time */
}
