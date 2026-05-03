#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# install.sh — install tesla_fan_control binary, default config, and systemd unit.
#
# Standard usage (real install, requires root):
#   sudo ./install.sh
#
# Sandboxed-install testing (no root required, no /dev or /etc probing):
#   PREFIX=/tmp/tfc-test ./install.sh
#
# PREFIX is rooted at the supplied directory: binary → $PREFIX/usr/local/sbin,
# config → $PREFIX/etc, unit → $PREFIX/etc/systemd/system. Prerequisite checks
# and `systemctl daemon-reload` are skipped in PREFIX mode.
#
# Prerequisite gates (refuse install) and warnings are documented in
# docs/PLAN.md §Deployment prerequisites and ADR-0001.

set -euo pipefail

PREFIX="${PREFIX:-}"

BIN_DEST="${PREFIX}/usr/local/sbin/tesla_fan_control"
CONFIG_DEST="${PREFIX}/etc/tesla_fan_control.conf"
UNIT_DEST="${PREFIX}/etc/systemd/system/tesla_fan_control.service"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG_SRC="${SCRIPT_DIR}/config/tesla_fan_control.conf"
UNIT_SRC="${SCRIPT_DIR}/systemd/tesla_fan_control.service"

err() { printf 'install.sh: %s\n' "$*" >&2; }
info() { printf '%s\n' "$*"; }

check_runtime_watchdog_sec() {
    # systemd's RuntimeWatchdogSec= claims /dev/watchdog itself; coexisting
    # with the daemon is impossible. Refuse if any conf file sets a non-zero,
    # non-empty value.
    local files=(/etc/systemd/system.conf)
    if compgen -G "/etc/systemd/system.conf.d/*.conf" > /dev/null; then
        # shellcheck disable=SC2206  # word-splitting is intended; paths have no spaces
        files+=( /etc/systemd/system.conf.d/*.conf )
    fi

    local f line value
    for f in "${files[@]}"; do
        [[ -r "$f" ]] || continue
        # Pull the last uncommented assignment in the file.
        line="$(grep -E '^[[:space:]]*RuntimeWatchdogSec[[:space:]]*=' "$f" | tail -n1 || true)"
        [[ -n "$line" ]] || continue
        value="${line#*=}"
        # Trim whitespace.
        value="${value#"${value%%[![:space:]]*}"}"
        value="${value%"${value##*[![:space:]]}"}"
        if [[ -n "$value" && "$value" != "0" && "$value" != "off" ]]; then
            err "RuntimeWatchdogSec=${value} is set in ${f}."
            err "systemd would claim /dev/watchdog and conflict with the daemon."
            err "Unset (RuntimeWatchdogSec=0 or remove the line) and rerun."
            err "See docs/PLAN.md §Deployment prerequisites and ADR-0001."
            exit 1
        fi
    done
}

check_watchdog_device_free() {
    if ! command -v lsof > /dev/null 2>&1; then
        info "warning: lsof not found; cannot verify /dev/watchdog* is unclaimed."
        return 0
    fi

    local holders
    # lsof exits non-zero with no output when nothing matches — tolerate that.
    holders="$(lsof /dev/watchdog* 2>/dev/null || true)"
    if [[ -n "$holders" ]]; then
        err "/dev/watchdog* is already held by another process:"
        printf '%s\n' "$holders" >&2
        err "Stop the holder before installing tesla_fan_control."
        exit 1
    fi
}

warn_nowayout() {
    local module_globs=(
        /sys/module/softdog/parameters/nowayout
        /sys/module/iTCO_wdt/parameters/nowayout
        /sys/module/nct6795_wdt/parameters/nowayout
        /sys/module/sp5100_tco/parameters/nowayout
        /sys/module/it87_wdt/parameters/nowayout
    )
    # Glob fallback for any other *_wdt* module.
    if compgen -G "/sys/module/*_wdt*/parameters/nowayout" > /dev/null; then
        # shellcheck disable=SC2206  # intentional word-splitting; sysfs paths have no spaces
        module_globs+=( /sys/module/*_wdt*/parameters/nowayout )
    fi

    local seen=""
    local p val
    for p in "${module_globs[@]}"; do
        [[ -r "$p" ]] || continue
        # Avoid duplicate warnings for paths that match both an explicit entry
        # and the glob fallback.
        case ":${seen}:" in
            *":${p}:"*) continue ;;
        esac
        seen="${seen}:${p}"
        val="$(cat "$p" 2>/dev/null || true)"
        if [[ "$val" == "1" || "$val" == "Y" ]]; then
            info ""
            info "warning: ${p} reads ${val}."
            info "  nowayout=1 means graceful disarm via 'V' will not stop a reboot."
            info "  Recommend setting nowayout=0 (module parameter or modprobe.d)."
            info ""
        fi
    done
}

locate_binary() {
    local candidates=(
        "${SCRIPT_DIR}/target/x86_64-unknown-linux-musl/release/tesla_fan_control"
        "${SCRIPT_DIR}/target/release/tesla_fan_control"
    )
    local c
    for c in "${candidates[@]}"; do
        if [[ -x "$c" ]]; then
            printf '%s\n' "$c"
            return 0
        fi
    done
    err "no built binary found. Looked in:"
    for c in "${candidates[@]}"; do
        err "  $c"
    done
    err "Run: cargo build --release --target x86_64-unknown-linux-musl"
    err "  (or: cargo build --release)"
    exit 1
}

main() {
    [[ -f "$CONFIG_SRC" ]] || { err "config source missing: $CONFIG_SRC"; exit 1; }
    [[ -f "$UNIT_SRC" ]] || { err "unit source missing: $UNIT_SRC"; exit 1; }

    if [[ -n "$PREFIX" ]]; then
        info "PREFIX=${PREFIX} — sandboxed install; skipping prereq checks (no /dev or /etc probing)."
    else
        if [[ "$(id -u)" != "0" ]]; then
            err "must run as root for a real install. Try: sudo $0"
            err "(set PREFIX=<dir> for a sandboxed test install without root.)"
            exit 1
        fi
        check_runtime_watchdog_sec
        check_watchdog_device_free
        warn_nowayout
    fi

    local binary
    binary="$(locate_binary)"
    info "Using binary: ${binary}"

    install -d "$(dirname "$BIN_DEST")"
    install -d "$(dirname "$CONFIG_DEST")"
    install -d "$(dirname "$UNIT_DEST")"

    install -m 0755 "$binary" "$BIN_DEST"
    info "installed binary → ${BIN_DEST}"

    if [[ -e "$CONFIG_DEST" ]]; then
        info "config already present at ${CONFIG_DEST} (existing config preserved)"
    else
        install -m 0644 "$CONFIG_SRC" "$CONFIG_DEST"
        info "installed default config → ${CONFIG_DEST}"
    fi

    install -m 0644 "$UNIT_SRC" "$UNIT_DEST"
    info "installed unit → ${UNIT_DEST}"

    if [[ -z "$PREFIX" ]]; then
        systemctl daemon-reload
        info "systemctl daemon-reload completed."
    else
        info "PREFIX set; skipping systemctl daemon-reload."
    fi

    info ""
    info "Install complete."
    info ""
    info "First-time setup:"
    info "  1. Edit ${CONFIG_DEST} for your hardware (chip, pwm_channel, RPM bounds)."
    info "  2. Run 'sudo tesla_fan_control --calibrate-fans' to find safe min_fan_pct"
    info "     and spin_up_grace_s values, then update the config."
    info "  3. Validate: 'sudo tesla_fan_control --check-config -c ${CONFIG_DEST}'"
    info "  4. Enable + start: 'sudo systemctl enable --now tesla_fan_control'"
}

main "$@"
