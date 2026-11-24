#!/bin/sh
# Check the pa1q board overlay: compile it, read the numbers back, and assert them.
#
#   board/check.sh                     the overlay alone
#   board/check.sh --firmware FILE     also re-derive from the phone's firmware
#
# FILE is the partition that carries the Sun graphics DTB (the dump's
# `partitions/vm-bootsys_a`). The firmware mode finds the FDT that contains the
# PA1 panel string, extracts it, and checks that the overlay's panel facts come
# from it. It is optional because the dump is not in this repository.
#
# Needs dtc. Nothing here touches a phone.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
dts="$here/pa1q-overlay.dts"
out=${OUT:-"$here/out"}
dtc=${DTC:-dtc}

command -v "$dtc" >/dev/null 2>&1 || { echo "board: missing $dtc" >&2; exit 1; }
mkdir -p "$out"

"$dtc" -I dts -O dtb -o "$out/pa1q-overlay.dtb" "$dts" 2>"$out/dtc.log" || {
    cat "$out/dtc.log"
    echo "board: FAIL (the overlay does not compile)" >&2
    exit 1
}
"$dtc" -I dtb -O dts -o "$out/pa1q-overlay.check.dts" "$out/pa1q-overlay.dtb" 2>/dev/null

fail=0
# need PATTERN COUNT WHAT
need() {
    n=$(grep -c -F -- "$1" "$out/pa1q-overlay.check.dts" || true)
    if [ "$n" -ne "$2" ]; then
        echo "FAIL: $3: want $2 of '$1', found $n"
        fail=1
    fi
}

# Panel: the part number, the geometry, and all seven refresh rates.
need 'qcom,mdss-dsi-panel-name = "PA1_S6E3FAE_AMB616FL03";' 1 "panel name"
need 'qcom,mdss-dsi-panel-width = <0x438>;' 7 "panel width 1080"
need 'qcom,mdss-dsi-panel-height = <0x924>;' 7 "panel height 2340"
for hz in 0x78 0x50 0x3c 0x30 0x1e 0x18 0x0a; do
    need "qcom,mdss-dsi-panel-framerate = <$hz>;" 1 "refresh $hz Hz"
done
# DSC: two encoders, 540-pixel slices, 60-row slices.
need 'qcom,mdss-dsc-slice-width = <0x21c>;' 7 "DSC slice width 540"
need 'qcom,mdss-dsc-slice-height = <0x3c>;' 7 "DSC slice height 60"
need 'qcom,lm-split = <0x21c 0x21c>;' 7 "DSC split 540/540"
need 'qcom,mdss-dsi-panel-type = "dsi_cmd_mode";' 1 "command mode"

# Panel reset line: TLMM gpio 98, active low.
need 'qcom,platform-reset-gpio = <0x07 0x62 0x00>;' 1 "panel reset gpio 98"

# Touch: STMicroelectronics on QUPv3 SE4, CS 51, irq 162, reset 161.
need 'compatible = "stm_spi";' 1 "touch compatible stm_spi"
need 'gpio_spi_cs = <0x07 0x33 0x00>;' 1 "touch CS gpio 51"
need 'gpio_irq = <0x07 0xa2 0x2001>;' 1 "touch irq gpio 162"
need 'max_coords = <0x1000 0x1000>;' 1 "touch 4096x4096"

# Rails: vddio 1.8, vdd 1.0-1.1, vci 3.0, tvdd 1.8.
need 'qcom,supply-name = "vddio";' 1 "vddio rail"
need 'qcom,supply-name = "vdd";' 1 "vdd rail"
need 'qcom,supply-name = "vci";' 1 "vci rail"
need 'qcom,supply-name = "tvdd";' 1 "tvdd rail"
need 'qcom,supply-min-voltage = <0x1b7740>;' 2 "1.8 V rails"
need 'qcom,supply-min-voltage = <0xf4240>;' 1 "vdd 1.0 V min"
need 'qcom,supply-max-voltage = <0x10c8e0>;' 1 "vdd 1.1 V max"
need 'qcom,supply-min-voltage = <0x2dc6c0>;' 1 "vci 3.0 V"

if [ "${1:-}" = "--firmware" ]; then
    src=${2:-}
    [ -n "$src" ] && [ -f "$src" ] || { echo "board: --firmware needs a file" >&2; exit 1; }

    # The panel string also appears as a filename in a filesystem image inside the
    # same partition, so a string hit is not enough. Walk every FDT magic instead,
    # and take the tree whose own bytes contain the panel node.
    magic=$(printf '\320\015\376\355')
    best=""
    for off in $(grep -aob -F -- "$magic" "$src" | cut -d: -f1); do
        size=$(od -An -tu4 --endian=big -j $((off + 4)) -N4 "$src" | tr -d ' \n')
        [ -n "$size" ] || continue
        [ "$size" -ge 8 ] && [ "$size" -le 50000000 ] || continue
        if dd if="$src" bs=1 skip="$off" count="$size" status=none | grep -a -q -m1 "PA1_S6E3FAE"; then
            best=$off
            bestsize=$size
        fi
    done
    [ -n "$best" ] || { echo "FAIL: no FDT carries the PA1 panel node"; exit 1; }

    dd if="$src" of="$out/pa1q-firmware.dtb" bs=1 skip="$best" count="$bestsize" status=none
    "$dtc" -I dtb -O dts -o "$out/pa1q-firmware.dts" "$out/pa1q-firmware.dtb" 2>/dev/null || {
        echo "FAIL: the enclosing FDT does not parse"
        exit 1
    }
    grep -q -F 'qcom,mdss-dsi-panel-name = "PA1_S6E3FAE_AMB616FL03";' "$out/pa1q-firmware.dts" || {
        echo "FAIL: firmware DTB has no PA1 panel node"
        exit 1
    }
    grep -q -F 'qcom,mdss-dsi-panel-width = <0x438>;' "$out/pa1q-firmware.dts" || {
        echo "FAIL: firmware panel is not 1080 wide"
        exit 1
    }
    grep -q -F 'qcom,mdss-dsi-panel-height = <0x924>;' "$out/pa1q-firmware.dts" || {
        echo "FAIL: firmware panel is not 2340 high"
        exit 1
    }
    for hz in 0x78 0x50 0x3c 0x30 0x1e 0x18 0x0a; do
        grep -q -F "qcom,mdss-dsi-panel-framerate = <$hz>;" "$out/pa1q-firmware.dts" || {
            echo "FAIL: firmware panel has no $hz Hz mode"
            exit 1
        }
    done
    echo "board: firmware at offset $best ($bestsize bytes) carries the same panel"
fi

if [ "$fail" -ne 0 ]; then
    echo "board: FAIL" >&2
    exit 1
fi
echo "board: pass (panel 1080x2340, 7 modes, DSC 540/60, touch SE4, 4 rails)"
