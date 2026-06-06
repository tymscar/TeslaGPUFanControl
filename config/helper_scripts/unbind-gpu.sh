#!/usr/bin/env bash
# unbind-gpu.sh — unbind one or more NVIDIA GPUs from the kernel driver,
# dropping them to PCIe-slot baseline power (Lever 5 in
# docs/IDLE-POWER-TUNING.md). Saves ~15–20 W per card. Reversible by
# rebinding.
#
# Use case: you have N P100s but only need M < N at any given time.
# Unbind the spares; rebind when needed.
#
# !!! REFUSES TO RUN WHILE tesla_fan_control IS ACTIVE !!!
#
# An unbound GPU disappears from NVML. The daemon would see consecutive
# NVML read failures, hit gpu_fail_threshold, declare Fault::GpuNvml, slam
# every fan to 100 % and stop feeding the watchdog → kernel reboot. To use
# this lever cleanly:
#
#   1. Stop the daemon: sudo systemctl stop tesla_fan_control
#   2. Edit /etc/tesla_fan_control.conf to remove the [gpu:N] block AND its
#      group reference for each card you intend to unbind.
#   3. Validate: tesla_fan_control --check-config -c /etc/tesla_fan_control.conf
#   4. Start the daemon: sudo systemctl start tesla_fan_control
#   5. Then run this script.
#
# Reversal is symmetric: rebind, restore the config, restart the daemon.
#
# Requires root.
#
# Usage:
#   sudo ./unbind-gpu.sh 0000:0X:00.0 [0000:0Y:00.0 ...]
#   sudo ./unbind-gpu.sh --rebind 0000:0X:00.0 [0000:0Y:00.0 ...]
#   sudo ./unbind-gpu.sh --list                          # show bus IDs

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "error: must run as root" >&2
    exit 1
fi

ACTION=unbind
BUSES=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --rebind) ACTION=rebind; shift ;;
        --list)   ACTION=list;   shift ;;
        --help|-h) sed -n '2,30p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) BUSES+=("$1"); shift ;;
    esac
done

if [[ $ACTION == list ]]; then
    echo "NVIDIA GPUs currently visible to nvidia-smi:"
    nvidia-smi --query-gpu=index,name,pci.bus_id,driver_model.current --format=csv 2>/dev/null \
        || echo "(nvidia-smi not available; driver may already be unbound for all)"
    echo
    echo "Devices currently bound to the nvidia driver:"
    if [[ -d /sys/bus/pci/drivers/nvidia ]]; then
        for d in /sys/bus/pci/drivers/nvidia/0000:*; do
            [[ -e $d ]] && echo "  $(basename "$d")"
        done
    else
        echo "  (no nvidia driver loaded?)"
    fi
    exit 0
fi

if [[ ${#BUSES[@]} -eq 0 ]]; then
    echo "error: no bus IDs given. Run with --list to see them." >&2
    exit 1
fi

if [[ $ACTION == unbind ]] \
   && systemctl is-active --quiet tesla_fan_control 2>/dev/null; then
    cat <<EOF >&2
error: tesla_fan_control.service is active.

Unbinding a GPU while the daemon is monitoring it would trip the
GPU NVML fault tracker (gpu_fail_threshold consecutive read failures →
Fault::GpuNvml → slam fans to 100 % → kernel reboot via watchdog).

Required prep: stop the daemon, remove the GPU's [gpu:N] block AND its
group reference from the config, validate, restart the daemon. Then run
this script.

See docs/IDLE-POWER-TUNING.md §"Lever 5 — Unbind unused cards" for the
full sequence.
EOF
    exit 1
fi

driver_dir=/sys/bus/pci/drivers/nvidia
if [[ ! -d $driver_dir ]]; then
    echo "error: $driver_dir does not exist (nvidia driver not loaded)" >&2
    exit 1
fi

action_file="${driver_dir}/${ACTION}"

for bus in "${BUSES[@]}"; do
    # Normalize: accept '0000:01:00.0' or '01:00.0'
    if [[ $bus != 0000:* ]]; then
        bus="0000:${bus}"
    fi
    if [[ $ACTION == unbind && ! -e ${driver_dir}/${bus} ]]; then
        echo "skip $bus: not currently bound to nvidia"
        continue
    fi
    if [[ $ACTION == rebind && -e ${driver_dir}/${bus} ]]; then
        echo "skip $bus: already bound to nvidia"
        continue
    fi
    echo "${ACTION} $bus..."
    echo "$bus" > "$action_file"
done

echo
echo "Result:"
for d in "$driver_dir"/0000:*; do
    [[ -e $d ]] && echo "  bound: $(basename "$d")"
done | sort
