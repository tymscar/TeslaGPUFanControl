#!/usr/bin/env bash
# measure-idle-power.sh — snapshot per-GPU idle power, clocks, ECC, and
# persistence state. Use as the before/after marker when trying any of the
# idle-power tuning levers in docs/IDLE-POWER-TUNING.md.
#
# Read-only, runnable as a non-root user (NVML reads don't require root).
#
# Usage:
#   ./measure-idle-power.sh                # one-shot snapshot
#   ./measure-idle-power.sh 30             # average power.draw over 30 s
#   ./measure-idle-power.sh --label before # tag the snapshot for diffing
#
# Tip: run with the GPU genuinely idle. Anything that opens NVML (including
# `nvidia-smi` itself running in a tight loop) wakes the card briefly.

set -euo pipefail

DURATION_S=0
LABEL=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --label)
            LABEL=$2
            shift 2
            ;;
        --label=*)
            LABEL=${1#*=}
            shift
            ;;
        --help|-h)
            sed -n '2,15p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            DURATION_S=$1
            shift
            ;;
    esac
done

if ! command -v nvidia-smi >/dev/null 2>&1; then
    echo "error: nvidia-smi not found in PATH" >&2
    exit 1
fi

ts=$(date -Iseconds)
header="GPU idle snapshot @ $ts"
[[ -n $LABEL ]] && header+=" [label=$LABEL]"
echo "$header"
echo

# Wide column set covers every lever's relevant state.
nvidia-smi \
    --query-gpu=index,name,pci.bus_id,power.draw,power.limit,power.default_limit,clocks.gr,clocks.mem,memory.used,utilization.gpu,ecc.mode.current,persistence_mode,driver_model.current \
    --format=csv

# nvidia-persistenced status (different from `-pm 1` — the daemon is the
# supported path and is a separate process).
echo
if systemctl is-active --quiet nvidia-persistenced 2>/dev/null; then
    echo "nvidia-persistenced.service: active"
else
    echo "nvidia-persistenced.service: NOT active (consider enabling)"
fi

# PCIe ASPM policy — affects link-level idle power.
if [[ -r /sys/module/pcie_aspm/parameters/policy ]]; then
    echo "pcie_aspm policy:           $(cat /sys/module/pcie_aspm/parameters/policy)"
fi

# Per-card PCI runtime PM state — only set if the runtime PM lever is in use.
echo
echo "Per-card PCI power/control state:"
while IFS=, read -r idx bus; do
    bus=${bus// /}
    bus=${bus#0000}
    bus=${bus#:}
    bus="0000:${bus}"
    ctrl="/sys/bus/pci/devices/${bus}/power/control"
    rt="/sys/bus/pci/devices/${bus}/power/runtime_status"
    if [[ -r $ctrl ]]; then
        printf "  GPU %s (%s): control=%s runtime_status=%s\n" \
            "$idx" "$bus" "$(cat "$ctrl")" "$(cat "$rt" 2>/dev/null || echo n/a)"
    fi
done < <(nvidia-smi --query-gpu=index,pci.bus_id --format=csv,noheader)

if [[ $DURATION_S -gt 0 ]]; then
    echo
    echo "Averaging power.draw over ${DURATION_S}s (sampling at 1 Hz)..."
    sum=0
    samples=0
    end=$(( $(date +%s) + DURATION_S ))
    while [[ $(date +%s) -lt $end ]]; do
        # Sum of all GPUs' power draw at this instant. Strip " W" suffix.
        instant=$(nvidia-smi --query-gpu=power.draw --format=csv,noheader,nounits \
                  | awk '{s+=$1} END {print s}')
        sum=$(awk -v s="$sum" -v i="$instant" 'BEGIN{print s+i}')
        samples=$((samples+1))
        sleep 1
    done
    if [[ $samples -gt 0 ]]; then
        avg=$(awk -v s="$sum" -v n="$samples" 'BEGIN{printf "%.2f", s/n}')
        echo "Average total power.draw across all GPUs: ${avg} W (${samples} samples)"
    fi
fi
