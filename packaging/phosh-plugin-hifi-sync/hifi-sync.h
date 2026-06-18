/*
 * Copyright (C) 2026 Anton
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

#pragma once

/* Phosh's quick-setting base class. pmOS phosh-dev does not install this header,
 * so it is vendored under phosh-vendor/ (added to the include path in meson.build);
 * it transitively pulls in PhoshStatusIcon / PhoshStatusPage used below. */
#include "quick-setting.h"

G_BEGIN_DECLS

#define PHOSH_TYPE_HIFI_SYNC_QUICK_SETTING (phosh_hifi_sync_quick_setting_get_type ())

G_DECLARE_FINAL_TYPE (PhoshHifiSyncQuickSetting,
                      phosh_hifi_sync_quick_setting,
                      PHOSH, HIFI_SYNC_QUICK_SETTING,
                      PhoshQuickSetting)

G_END_DECLS
