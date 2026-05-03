#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# uninstall.sh — stop, disable, and remove the tesla_fan_control service.
#
# Removes:
#   - /usr/local/sbin/tesla_fan_control
#   - /etc/systemd/system/tesla_fan_control.service
#
# Preserved by default:
#   - /etc/tesla_fan_control.conf
#
# Operator edits to the config file are valuable; uninstall leaves it in
# place. Run `sudo rm /etc/tesla_fan_control.conf` afterwards for a full
# purge.
#
# A reboot after uninstall is recommended: the daemon flips PWM back to
# the snapshotted register on shutdown, but BIOS-managed PWM init may
# have changed since boot — a reboot reinitialises PWM cleanly. See
# docs/PLAN.md §Fan restore policy.

set -euo pipefail

BIN_PATH="/usr/local/sbin/tesla_fan_control"
UNIT_PATH="/etc/systemd/system/tesla_fan_control.service"

info() { printf '%s\n' "$*"; }
err() { printf 'uninstall.sh: %s\n' "$*" >&2; }

if [[ "$(id -u)" != "0" ]]; then
    err "must run as root. Try: sudo $0"
    exit 1
fi

systemctl stop tesla_fan_control 2>/dev/null || true
systemctl disable tesla_fan_control 2>/dev/null || true

if [[ -e "$BIN_PATH" ]]; then
    rm -f "$BIN_PATH"
    info "removed ${BIN_PATH}"
else
    info "binary not present at ${BIN_PATH} (skipping)"
fi

if [[ -e "$UNIT_PATH" ]]; then
    rm -f "$UNIT_PATH"
    info "removed ${UNIT_PATH}"
else
    info "unit not present at ${UNIT_PATH} (skipping)"
fi

systemctl daemon-reload
info "systemctl daemon-reload completed."

info ""
info "Uninstall complete."
info "  /etc/tesla_fan_control.conf preserved (rm it manually for a full purge)."
info "  reboot recommended to resync BIOS PWM init"
