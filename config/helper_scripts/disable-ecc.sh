#!/usr/bin/env bash
# disable-ecc.sh — disable ECC on every NVIDIA GPU on the box (Lever 1 in
# docs/IDLE-POWER-TUNING.md). Saves ~2–5 W per Pascal Tesla card at idle by
# letting memory enter deeper power states. Reversible.
#
# Trade-off: ECC catches single-bit memory errors. Disable only if your
# workload can tolerate that (re-runnable training/inference: yes; long
# scientific compute: probably not).
#
# Requires root. ECC mode change does not take effect until the GPU is
# reset; the simplest correct path is a reboot. The script does NOT reboot
# automatically — it prints the next step and exits.
#
# Usage:
#   sudo ./disable-ecc.sh           # disable on every GPU
#   sudo ./disable-ecc.sh -i 0      # disable on GPU 0 only
#   sudo ./disable-ecc.sh --revert  # re-enable ECC on every GPU
#
# Verify after reboot:
#   nvidia-smi --query-gpu=index,ecc.mode.current --format=csv

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "error: must run as root (ECC mode change requires it)" >&2
    exit 1
fi

if ! command -v nvidia-smi >/dev/null 2>&1; then
    echo "error: nvidia-smi not found in PATH" >&2
    exit 1
fi

REVERT=0
SMI_ARGS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --revert)
            REVERT=1
            shift
            ;;
        --help|-h)
            sed -n '2,17p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            SMI_ARGS+=("$1")
            shift
            ;;
    esac
done

mode=0
target_str="disabled"
if [[ $REVERT -eq 1 ]]; then
    mode=1
    target_str="enabled"
fi

echo "Setting ECC ${target_str} (mode=${mode}) on requested GPU(s)..."
nvidia-smi -e "$mode" "${SMI_ARGS[@]}"

cat <<EOF

ECC mode change is staged. The GPU must be reset for it to take effect.
The cleanest way is:

    sudo reboot

After reboot, verify with:

    nvidia-smi --query-gpu=index,ecc.mode.current,ecc.mode.pending --format=csv

Both columns should read "${target_str^}". If they differ, the change is
still pending another reset.
EOF
