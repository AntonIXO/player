// SPDX-License-Identifier: GPL-2.0-only
/* Compatibility module for the existing Poco F1 7.2.3 audio kernel.
 * Mirrors Qualcomm smb-lib's _smblib_vbus_regulator_enable(): inhibit the
 * buck/boost 1-in-8 mode while sourcing OTG VBUS. Otherwise blanking the
 * panel latches OTG failure although CMD_OTG and regulator state stay on.
 * Load before enabling VBUS; unload after disabling it in device mode.
 * The proper driver patch in the parent directory supersedes this module.
 */
#include <linux/module.h>
#include <linux/platform_device.h>
#include <linux/regmap.h>
#include <linux/of.h>
static struct device *dev;
static struct regmap *map;
static unsigned int saved;
static int set_halt_mode(unsigned int value)
{
 int ret = regmap_write(map, 0x11d0, 0xa5);
 if (ret) return ret;
 return regmap_update_bits(map, 0x11c0, BIT(0), value);
}
static int __init quirk_init(void)
{
 int ret;
 unsigned int actual;
 if (!of_machine_is_compatible("xiaomi,beryllium")) return -ENODEV;
 dev = bus_find_device_by_name(&platform_bus_type, NULL,
   "c440000.spmi:pmic@2:usb-vbus-regulator@1100");
 if (!dev) return -ENODEV;
 map = dev_get_regmap(dev->parent, NULL);
 if (!map) { ret = -ENODEV; goto fail; }
 ret = regmap_read(map, 0x11c0, &saved);
 if (ret) goto fail;
 ret = set_halt_mode(BIT(0));
 if (ret) goto fail;
 ret = regmap_read(map, 0x11c0, &actual);
 if (ret || !(actual & BIT(0))) { set_halt_mode(saved); ret = -EIO; goto fail; }
 pr_info("pmi8998_otg_fix: halt-1-in-8 enabled, previous=%x current=%x\n", saved, actual);
 return 0;
fail:
 put_device(dev);
 return ret;
}
static void __exit quirk_exit(void)
{
 set_halt_mode(saved);
 put_device(dev);
}
module_init(quirk_init);
module_exit(quirk_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Poco F1 PMI8998 OTG halt-1-in-8 compatibility fix");
