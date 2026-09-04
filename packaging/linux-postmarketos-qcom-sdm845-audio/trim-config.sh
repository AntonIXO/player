#!/bin/sh
# Regenerate config-postmarketos-qcom-sdm845.aarch64 as "upstream pmOS config minus
# what a Poco F1 bit-perfect DAP never uses".
#
# Run it inside a patched linux-$pkgver tree (tarball + 0001..0009 + ftrace patch),
# with the upstream pmaports config already copied to .config:
#
#   cp .../config-postmarketos-qcom-sdm845.aarch64 .config
#   sh trim-config.sh
#   make ARCH=arm64 LLVM=1 olddefconfig
#
# Rationale per group is inline. Everything here is build-time/size only; nothing
# in it touches the USB-audio path (snd-usb-audio, DWC3, ALSA core) or sound/,
# which are deliberately left exactly as upstream has them.
set -eu
C=./scripts/config

# --- debug info: DWARF for 16k+ objects, only useful for kernel debugging -----
$C --enable  DEBUG_INFO_NONE
$C --disable DEBUG_INFO_DWARF_TOOLCHAIN_DEFAULT
$C --disable DEBUG_INFO_REDUCED

# --- CFI off, ThinLTO kept. CFI needs LTO_CLANG, not the other way round, so
#     LTO_CLANG_THIN stays =y and only the indirect-call sanitiser goes. -------
$C --disable CFI
$C --disable CFI_ICALL_NORMALIZE_INTEGERS
$C --disable CFI_PERMISSIVE

# --- Bluetooth: not used by the player ----------------------------------------
$C --disable BT
$C --disable BT_HCIBTUSB
$C --disable BT_HCIUART
$C --disable BT_QCOMSMD

# --- media: no camera, no video decode, no IR on a DAP (178 modules, 140 of
#     them rc/keymaps) -----------------------------------------------------
$C --disable MEDIA_SUPPORT

# --- coresight/STM: SoC trace hardware, debug only ----------------------------
for s in CORESIGHT STM STM_PROTO_BASIC STM_PROTO_SYS_T; do $C --disable $s; done

# --- DRM: external bridge chips, other-vendor display IP, other-device panels.
#     beryllium is a direct DSI panel: novatek,nt36672a (=y, kept). ------------
for s in \
	DRM_HDLCD DRM_KOMEDA DRM_TIDSS DRM_STM DRM_STM_LVDS DRM_UDL DRM_GUD \
	DRM_PANTHOR DRM_POWERVR \
	DRM_DISPLAY_CONNECTOR DRM_SIMPLE_BRIDGE DRM_I2C_ADV7511 \
	DRM_ANALOGIX_ANX7625 DRM_CDNS_DSI DRM_CDNS_MHDP8546 DRM_SAMSUNG_DSIM \
	DRM_ITE_IT6263 DRM_ITE_IT66121 \
	DRM_LONTIUM_LT8912B DRM_LONTIUM_LT9611 DRM_LONTIUM_LT9611UXC DRM_LONTIUM_LT8713SX \
	DRM_TOSHIBA_TC358767 DRM_TOSHIBA_TC358768 DRM_TI_TFP410 DRM_TI_SN65DSI83 \
	DRM_PANEL_BOE_TV101WUM_NL6 DRM_PANEL_HIMAX_HX8279 DRM_PANEL_HIMAX_HX83112A \
	DRM_PANEL_HIMAX_HX83112B DRM_PANEL_ILITEK_ILI9882T DRM_PANEL_KHADAS_TS050 \
	DRM_PANEL_MANTIX_MLAF057WE51 DRM_PANEL_NOVATEK_NT36672E DRM_PANEL_NOVATEK_NT37801 \
	DRM_PANEL_RAYDIUM_RM692E5 DRM_PANEL_SAMSUNG_ATNA33XC20 DRM_PANEL_STARTEK_KD070FHFID015 \
	DRM_PANEL_SIMPLE DRM_PANEL_SUMMIT DRM_PANEL_TRULY_NT35597_WQXGA \
	DRM_PANEL_VISIONOX_RM69299 DRM_PANEL_VISIONOX_VTDR6130 \
	; do $C --disable $s; done

# --- qcom clock controllers for every SoC that is not sdm845 (68 modules).
#     SDM_LPASSCC_845 / SDM_CAMCC_845 stay. -----------------------------------
for s in \
	CLK_ELIZA_DISPCC CLK_ELIZA_TCSRCC CLK_GLYMUR_DISPCC CLK_GLYMUR_TCSRCC \
	CLK_KAANAPALI_CAMCC CLK_KAANAPALI_DISPCC CLK_KAANAPALI_GPUCC \
	CLK_KAANAPALI_TCSRCC CLK_KAANAPALI_VIDEOCC \
	CLK_X1E80100_CAMCC CLK_X1E80100_DISPCC CLK_X1E80100_GPUCC \
	CLK_X1P42100_CAMCC CLK_X1P42100_GPUCC CLK_X1P42100_VIDEOCC \
	CLK_QCM2290_GPUCC IPQ_CMN_PLL IPQ_NSSCC_5424 IPQ_NSSCC_9574 \
	MSM_MMCC_8994 MSM_MMCC_8996 MSM_MMCC_8998 QCM_DISPCC_2290 \
	QCS_DISPCC_615 QCS_CAMCC_615 QCS_GPUCC_615 QCS_VIDEOCC_615 \
	SA_CAMCC_8775P SA_DISPCC_8775P SA_GPUCC_8775P SA_VIDEOCC_8775P \
	SC_CAMCC_7280 SC_CAMCC_8280XP SC_DISPCC_7280 SC_DISPCC_8280XP \
	SC_GPUCC_7280 SC_GPUCC_8280XP SC_LPASSCC_8280XP SC_LPASS_CORECC_7280 \
	SC_VIDEOCC_7280 \
	SM_CAMCC_6350 SM_CAMCC_MILOS SM_CAMCC_8250 SM_CAMCC_8550 SM_CAMCC_8650 \
	SM_CAMCC_8750 SM_DISPCC_6115 SM_DISPCC_6350 SM_DISPCC_MILOS SM_DISPCC_8450 \
	SM_DISPCC_8550 SM_DISPCC_8750 SM_GPUCC_6115 SM_GPUCC_6350 SM_GPUCC_MILOS \
	SM_GPUCC_8350 SM_GPUCC_8450 SM_GPUCC_8550 SM_GPUCC_8650 SM_GPUCC_8750 \
	SM_TCSRCC_8750 SM_VIDEOCC_6350 SM_VIDEOCC_MILOS SM_VIDEOCC_8450 \
	SM_VIDEOCC_8550 SM_VIDEOCC_8750 CLK_GFM_LPASS_SM8250 \
	; do $C --disable $s; done

# --- HID: keep only generic + multitouch; the rest is gamepad/keyboard/tablet
#     vendor quirks and the HID sensor hub (x86 tablets). ----------------------
for s in $(sed -n 's/^CONFIG_\(HID_[A-Z0-9_]*\)=m$/\1/p' .config); do
	case "$s" in
		HID_GENERIC|HID_MULTITOUCH) continue ;;
	esac
	$C --disable "$s"
done

# --- IIO: ChromeOS EC sensors, HID sensors, FPGA/other-SoC ADCs ---------------
for s in \
	IIO_CROS_EC_SENSORS_CORE IIO_CROS_EC_SENSORS IIO_CROS_EC_LIGHT_PROX \
	IIO_CROS_EC_BARO XILINX_XADC SOPHGO_CV1800B_ADC TI_ADS1015 \
	; do $C --disable $s; done

# --- wifi: beryllium is WCN3990 -> ath10k_snoc. Keep ath10k, drop the rest. ---
for s in \
	ATH11K ATH11K_AHB ATH11K_PCI ATH12K WCN36XX \
	IWLWIFI IWLDVM IWLMVM \
	MWIFIEX MWIFIEX_SDIO MWIFIEX_PCIE MWIFIEX_USB \
	MT76_CORE MT76_CONNAC_LIB MT7921_COMMON MT7921E \
	; do $C --disable $s; done
