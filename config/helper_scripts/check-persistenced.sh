#!/usr/bin/env bash
# check-persistenced.sh — confirm nvidia-persistenced is the path holding
# the driver resident, not a manual `nvidia-smi -pm 1` (Lever 2 in
# docs/IDLE-POWER-TUNING.md).
#
# Both keep the driver loaded between jobs, but the persistenced daemon is
# the supported path and lets the firmware enter deeper P-states between
# jobs. A bare `-pm 1` keeps the driver up but interferes with idle
# transitions on Pascal Tesla.
#
# Read-only, runnable as a non-root user.
#
# Usage:
#   ./check-persistenced.sh

set -euo pipefail

if ! command -v nvidia-smi >/dev/null 2>&1; then
    echo "error: nvidia-smi not found in PATH" >&2
    exit 1
fi

# 1) Is the systemd unit running?
if systemctl is-active --quiet nvidia-persistenced 2>/dev/null; then
    persistenced_active=1
    echo "✓ nvidia-persistenced.service is active"
else
    persistenced_active=0
    echo "✗ nvidia-persistenced.service is NOT active"
    echo "  → sudo systemctl enable --now nvidia-persistenced"
fi

# 2) What does each GPU report for persistence_mode? "Enabled" can come
#    from either persistenced OR a manual `nvidia-smi -pm 1` — distinguish.
echo
echo "Per-GPU persistence_mode (per nvidia-smi):"
nvidia-smi --query-gpu=index,name,persistence_mode --format=csv

# 3) Is the persistenced PID actually running? On some distros the systemd
#    unit can be misconfigured and `is-active` can lie.
if pgrep -x nvidia-persistenced >/dev/null 2>&1; then
    pid=$(pgrep -x nvidia-persistenced | head -n1)
    echo
    echo "nvidia-persistenced PID: $pid"
fi

# 4) If persistenced isn't running but persistence_mode is Enabled
#    everywhere, that's the signal someone ran a bare `-pm 1`.
if [[ $persistenced_active -eq 0 ]]; then
    enabled_count=$(nvidia-smi --query-gpu=persistence_mode --format=csv,noheader | grep -c -i "Enabled" || true)
    if [[ $enabled_count -gt 0 ]]; then
        echo
        echo "WARNING: persistence_mode is Enabled on $enabled_count GPU(s) but"
        echo "         nvidia-persistenced is not running. Likely someone ran"
        echo "         a manual 'nvidia-smi -pm 1'. Recommended:"
        echo "           sudo nvidia-smi -pm 0"
        echo "           sudo systemctl enable --now nvidia-persistenced"
    fi
fi
