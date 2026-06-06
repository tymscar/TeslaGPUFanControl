#!/usr/bin/env bash
# enable-pcie-aspm.sh — flip the kernel's PCIe ASPM policy to 'powersave'
# (Lever 3 in docs/IDLE-POWER-TUNING.md). When both endpoints support it,
# the PCIe link drops to L1 between transactions, saving 1–3 W per link.
#
# RISK: many server BIOSes disable ASPM for stability. With ASPM forced on,
# you may see PCIe AER (Advanced Error Reporting) events in dmesg or, on
# bad combinations, link retraining storms. Roll back at the first sign.
#
# Roll back:
#   sudo ./enable-pcie-aspm.sh --revert
#
# To make persistent across reboots, add `pcie_aspm=force` and
# `pcie_aspm.policy=powersave` to your kernel cmdline (GRUB or
# systemd-boot). This script does NOT modify your boot config — printing
# the recommendation only, since wrong cmdline edits can prevent boot.
#
# Requires root. Read /sys/module/pcie_aspm/parameters/policy to see what
# the kernel actually accepted; some kernels gate `powersave` and silently
# leave the policy at `performance`.
#
# Usage:
#   sudo ./enable-pcie-aspm.sh           # set powersave
#   sudo ./enable-pcie-aspm.sh --revert  # back to default
#   sudo ./enable-pcie-aspm.sh --status  # show current policy + AER event count

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "error: must run as root" >&2
    exit 1
fi

POLICY_FILE=/sys/module/pcie_aspm/parameters/policy
if [[ ! -w $POLICY_FILE ]]; then
    cat <<EOF >&2
error: $POLICY_FILE is not writable.

Either ASPM is not exposed by your kernel, or it has been disabled at boot
with 'pcie_aspm=off'. Check kernel cmdline:

    cat /proc/cmdline | tr ' ' '\\n' | grep -i aspm

If 'pcie_aspm=off' is present, ASPM cannot be flipped at runtime.
EOF
    exit 1
fi

ACTION=set
while [[ $# -gt 0 ]]; do
    case "$1" in
        --revert)  ACTION=revert ;;
        --status)  ACTION=status ;;
        --help|-h) sed -n '2,21p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "error: unknown arg $1" >&2; exit 1 ;;
    esac
    shift
done

show_status() {
    echo "Current pcie_aspm policy: $(cat "$POLICY_FILE")"
    if [[ -d /sys/kernel/debug/pcie_aspm ]]; then
        echo "(see /sys/kernel/debug/pcie_aspm/ for per-device link state)"
    fi
    # Count AER events since boot — high count after enabling powersave is
    # the smoke signal that this lever doesn't work on your board.
    aer=$(dmesg --time-format=iso 2>/dev/null \
          | grep -ciE 'pcieport.*PCIe Bus Error|AER:' || true)
    echo "PCIe AER events in dmesg since boot: $aer"
    if [[ $aer -gt 0 ]]; then
        echo "  → recent samples:"
        dmesg --time-format=iso 2>/dev/null \
            | grep -iE 'pcieport.*PCIe Bus Error|AER:' \
            | tail -n3 \
            | sed 's/^/    /'
    fi
}

case $ACTION in
    status)
        show_status
        ;;
    revert)
        echo "Reverting pcie_aspm policy to 'default'..."
        echo default > "$POLICY_FILE"
        show_status
        ;;
    set)
        echo "Setting pcie_aspm policy to 'powersave'..."
        echo powersave > "$POLICY_FILE"
        show_status
        cat <<EOF

Watch dmesg for the next several minutes:

    sudo dmesg -w | grep -iE 'pcieport|aer'

Any 'PCIe Bus Error' or sustained AER events → revert immediately:

    sudo $0 --revert

To make this persistent across reboots, append the following to your
kernel cmdline (in /etc/default/grub or your bootloader config — DO NOT
edit blindly; verify your bootloader first):

    pcie_aspm=force pcie_aspm.policy=powersave
EOF
        ;;
esac
