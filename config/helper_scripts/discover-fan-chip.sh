#!/usr/bin/env bash
# discover-fan-chip.sh — list every hwmon chip on this box and flag the ones
# usable as `chip = …` in tesla_fan_control.conf.
#
# A chip is usable iff it exposes at least one pwm* control file. Chips that
# only report temperature (coretemp, nvme, k10temp, …) are listed but skipped.
#
# For each usable chip the script prints:
#   • chip name (the string the daemon matches against)
#   • hwmon path (escape-hatch value for `hwmon_path =`)
#   • stable platform path (preferred for `device_path =` if hwmonN drifts)
#   • per-channel pwm and fan*_input snapshot, so you can tell which channels
#     are wired to a spinning fan right now
#
# Read-only: no /sys writes, runnable as a non-root user. To actually identify
# which physical fan is on which channel, follow up with the manual sweep in
# docs/CONFIG-TUTORIAL.md §3b (requires root).
#
# Usage:
#   ./config/helper_scripts/discover-fan-chip.sh
#   ./config/helper_scripts/discover-fan-chip.sh --all   # also show temp-only chips

set -euo pipefail

SHOW_ALL=0
case "${1:-}" in
    --all|-a) SHOW_ALL=1 ;;
    --help|-h)
        sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    "") ;;
    *)
        echo "unknown flag: $1" >&2
        echo "usage: $0 [--all]" >&2
        exit 2
        ;;
esac

HWMON_ROOT=/sys/class/hwmon

if [[ ! -d $HWMON_ROOT ]]; then
    echo "error: $HWMON_ROOT does not exist — is hwmon enabled in this kernel?" >&2
    exit 1
fi

shopt -s nullglob
hwmon_dirs=("$HWMON_ROOT"/hwmon*)
if [[ ${#hwmon_dirs[@]} -eq 0 ]]; then
    echo "no hwmon entries found under $HWMON_ROOT" >&2
    echo "load a SuperIO driver first, e.g.: sudo modprobe nct6775" >&2
    exit 1
fi

usable_count=0
suggested_chip=""
suggested_path=""

print_chip() {
    local hwmon_path=$1 name=$2 pwm_channels=$3 has_pwm=$4
    local device_link platform_path driver

    device_link=$(readlink -f "$hwmon_path/device" 2>/dev/null || true)
    platform_path=$device_link
    driver=$(basename "$(readlink -f "$hwmon_path/device/driver" 2>/dev/null || echo unknown)")

    printf '\n[%s]\n' "$name"
    printf '  hwmon_path  = %s\n' "$hwmon_path"
    [[ -n $platform_path ]] && printf '  device_path = %s\n' "$platform_path"
    printf '  driver      = %s\n' "$driver"

    if [[ $has_pwm -eq 0 ]]; then
        printf '  pwm         = (none — temperature-only chip, not usable as fan controller)\n'
        return
    fi

    printf '  pwm         = %s\n' "$pwm_channels"
    printf '  channels:\n'
    local pwm
    for pwm in "$hwmon_path"/pwm[0-9]*; do
        # Skip pwm*_enable, pwm*_mode etc. — only bare pwmN.
        [[ $(basename "$pwm") =~ ^pwm[0-9]+$ ]] || continue
        local ch=${pwm##*pwm}
        local pwm_val fan_path fan_val enable_path enable_val
        pwm_val=$(cat "$pwm" 2>/dev/null || echo "?")
        fan_path="$hwmon_path/fan${ch}_input"
        fan_val="(no tach)"
        [[ -r $fan_path ]] && fan_val="$(cat "$fan_path") rpm"
        enable_path="$hwmon_path/pwm${ch}_enable"
        enable_val="?"
        [[ -r $enable_path ]] && enable_val=$(cat "$enable_path")
        printf '    pwm%-2s  duty=%-3s/255  %-12s  pwm_enable=%s\n' \
            "$ch" "$pwm_val" "$fan_val" "$enable_val"
    done
}

for d in "${hwmon_dirs[@]}"; do
    [[ -r $d/name ]] || continue
    name=$(<"$d/name")

    pwm_files=("$d"/pwm[0-9]*)
    pwm_channels=""
    for f in "${pwm_files[@]}"; do
        bn=$(basename "$f")
        [[ $bn =~ ^pwm[0-9]+$ ]] || continue
        pwm_channels+="${bn#pwm} "
    done
    pwm_channels=${pwm_channels% }

    if [[ -n $pwm_channels ]]; then
        has_pwm=1
        usable_count=$((usable_count + 1))
        if [[ -z $suggested_chip ]]; then
            suggested_chip=$name
            suggested_path=$d
        fi
    else
        has_pwm=0
        [[ $SHOW_ALL -eq 0 ]] && continue
    fi

    print_chip "$d" "$name" "$pwm_channels" "$has_pwm"
done

echo
echo "──────────────────────────────────────────────────────────────"
if [[ $usable_count -eq 0 ]]; then
    echo "No PWM-capable chips found."
    echo
    echo "Likely fix: load the SuperIO driver matching your motherboard, e.g."
    echo "  sudo modprobe nct6775     # most Intel boards (nct6798, nct6796, …)"
    echo "  sudo modprobe it87        # ITE chips (it8628, it8772, …)"
    echo "  sudo modprobe f71882fg    # Fintek"
    echo "  sudo modprobe nct6683     # some AMD boards"
    echo
    echo "If 'no supported chip', try: sudo modprobe nct6775 force_id=0xd428"
    echo "(force_id from dmidecode / vendor docs)."
    exit 1
fi

echo "Found $usable_count PWM-capable chip(s)."
echo
echo "Suggested config snippet (copy into your [fan:*] block):"
echo
echo "  chip        = $suggested_chip"
echo "  # device_path = $(readlink -f "$suggested_path/device" 2>/dev/null || echo "$suggested_path")"
echo "  # hwmon_path  = $suggested_path     # last-resort literal pin (hwmonN can drift)"
echo
echo "Next steps:"
echo "  1. Pick pwm_channel by sweeping each channel — see"
echo "     docs/CONFIG-TUTORIAL.md §3b (requires root)."
echo "  2. Validate the resulting config:"
echo "     ./target/release/tesla_fan_control --check-config -c <your.conf>"
