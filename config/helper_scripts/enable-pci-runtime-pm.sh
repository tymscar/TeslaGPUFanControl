#!/usr/bin/env bash
# enable-pci-runtime-pm.sh — flip /sys/bus/pci/devices/<gpu>/power/control
# to 'auto' on every NVIDIA GPU, allowing the kernel to suspend the device
# to D3hot when idle (Lever 4 in docs/IDLE-POWER-TUNING.md).
#
# Potential idle-power saving on Pascal Tesla: 8–15 W per card. Real, but:
#
# !!! INCOMPATIBLE WITH tesla_fan_control AS CONFIGURED !!!
#
# The daemon polls NVML every poll_interval_ms (default 1000). Every poll
# is a device access that wakes the GPU from D3hot, defeating the lever.
# To use this, you must EITHER:
#   1) Stop tesla_fan_control entirely while testing this lever, OR
#   2) Increase poll_interval_ms to >= 10000 in your config and SIGHUP.
#      Note: this weakens thermal-blind fault detection; a stuck NVML read
#      will take 10× longer to escalate.
#
# Additional risks:
#   - The proprietary nvidia driver on Pascal is not consistently tested
#     with PCI runtime PM. Some kernel/driver combos hang on resume.
#   - Any other tool that opens NVML (monitoring agents, Prometheus
#     exporters, gpu_burn left running) will also defeat the savings.
#
# Roll back:
#   sudo ./enable-pci-runtime-pm.sh --revert
#
# Requires root.
#
# Usage:
#   sudo ./enable-pci-runtime-pm.sh            # set auto on every NVIDIA GPU
#   sudo ./enable-pci-runtime-pm.sh --revert   # set on (kernel-default)
#   sudo ./enable-pci-runtime-pm.sh --status   # show current state per GPU

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "error: must run as root" >&2
    exit 1
fi

if ! command -v nvidia-smi >/dev/null 2>&1; then
    echo "error: nvidia-smi not found in PATH" >&2
    exit 1
fi

ACTION=set
while [[ $# -gt 0 ]]; do
    case "$1" in
        --revert)  ACTION=revert ;;
        --status)  ACTION=status ;;
        --help|-h) sed -n '2,30p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "error: unknown arg $1" >&2; exit 1 ;;
    esac
    shift
done

# Refuse to change runtime PM while the daemon is polling — the change
# would apply but the lever wouldn't actually save power, and we'd just
# burn cycles on driver wake-ups.
if [[ $ACTION == set ]] && systemctl is-active --quiet tesla_fan_control 2>/dev/null; then
    cat <<EOF >&2
error: tesla_fan_control.service is active.

Its 1 Hz NVML polling will defeat PCI runtime PM (every poll wakes the GPU
from D3hot). Either stop the daemon first:

    sudo systemctl stop tesla_fan_control

or raise [global] poll_interval_ms to 10000 or more in the config and
reload (sudo systemctl reload tesla_fan_control), accepting the
correspondingly slower thermal-blind fault detection.

If you really want to proceed anyway, use --status first to inspect, or
run the underlying sysfs writes manually.
EOF
    exit 1
fi

target=auto
[[ $ACTION == revert ]] && target=on

iter_gpus() {
    nvidia-smi --query-gpu=index,pci.bus_id --format=csv,noheader \
        | while IFS=, read -r idx bus; do
        bus=${bus// /}
        # nvidia-smi formats bus IDs like 00000000:0X:00.0; sysfs wants 0000:0X:00.0
        bus=${bus#0000}
        bus=${bus#:}
        bus="0000:${bus}"
        printf "%s\t%s\n" "$idx" "$bus"
    done
}

case $ACTION in
    status)
        echo "Current PCI runtime PM state per GPU:"
        while IFS=$'\t' read -r idx bus; do
            ctrl="/sys/bus/pci/devices/${bus}/power/control"
            rt="/sys/bus/pci/devices/${bus}/power/runtime_status"
            if [[ -r $ctrl ]]; then
                printf "  GPU %s (%s): control=%s runtime_status=%s\n" \
                    "$idx" "$bus" "$(cat "$ctrl")" "$(cat "$rt" 2>/dev/null || echo n/a)"
            else
                printf "  GPU %s (%s): %s missing\n" "$idx" "$bus" "$ctrl"
            fi
        done < <(iter_gpus)
        ;;
    set|revert)
        echo "Setting power/control = ${target} on every NVIDIA GPU..."
        while IFS=$'\t' read -r idx bus; do
            ctrl="/sys/bus/pci/devices/${bus}/power/control"
            if [[ -w $ctrl ]]; then
                echo "$target" > "$ctrl"
                printf "  GPU %s (%s): %s\n" "$idx" "$bus" "$(cat "$ctrl")"
            else
                printf "  GPU %s (%s): %s not writable, skipped\n" "$idx" "$bus" "$ctrl"
            fi
        done < <(iter_gpus)
        if [[ $ACTION == set ]]; then
            cat <<EOF

Now wait at least 30 s with nothing accessing the GPUs, then check:

    sudo $0 --status

runtime_status should read 'suspended' on cards that are genuinely idle.
If it stays 'active' the lever isn't taking — usually because something
(a monitoring agent, persistenced) is keeping the card open.

If you see any nvidia driver hang / EXEC reset in dmesg, revert IMMEDIATELY:

    sudo $0 --revert
EOF
        fi
        ;;
esac
