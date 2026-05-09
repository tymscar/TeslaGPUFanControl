#!/usr/bin/env bash
# identify-pwm-channel.sh — physically identify which fan is on which PWM
# channel by stopping it for 10s, then spinning it to full duty.
#
# Pass a chip name (as it appears in /sys/class/hwmon/*/name) and a PWM
# channel number; the script takes manual control of that channel, drops
# duty to 0, waits 10s while polling the tach so you can hear / see which
# fan stops, then ramps to 255 and waits 5s so you can confirm the same fan
# is the one that spins back up.
#
# SAFETY GUARANTEES:
#   • Refuses to run if tesla_fan_control.service is active (mirrors the
#     daemon's --calibrate-fans guard in src/main.rs:558-563).
#   • Refuses to run if any NVIDIA GPU is already ≥ 70 °C (configurable via
#     --temp-cap N) — stopping a fan on a hot card can throttle / damage it.
#   • Always restores the original pwm value AND pwm_enable mode on exit,
#     including SIGINT / SIGTERM / errors (bash EXIT trap).
#   • Never runs against a channel not currently in BIOS auto mode (5)
#     unless you pass --force — protects against clobbering another tool
#     that has already taken manual control.
#
# Read-modify-write on /sys requires root.
#
# Usage:
#   sudo ./identify-pwm-channel.sh <chip-name> <pwm-channel> [options]
#
# Options:
#   --temp-cap N   refuse to run if any GPU is ≥ N °C (default 70)
#   --stop-secs N  seconds to hold pwm=0     (default 10)
#   --spin-secs N  seconds to hold pwm=255   (default 5)
#   --force        run even if pwm_enable is not 5 (BIOS auto)
#   --no-gpu-check skip the NVML temperature precheck
#   -h, --help     show this message
#
# Examples:
#   sudo ./identify-pwm-channel.sh nct6798 1
#   sudo ./identify-pwm-channel.sh nct6791 3 --stop-secs 15
#
# Tip: run the discover-fan-chip.sh script first to list candidate channels.

set -euo pipefail

# ── arg parsing ──────────────────────────────────────────────────────────
TEMP_CAP=70
STOP_SECS=10
SPIN_SECS=5
FORCE=0
GPU_CHECK=1
CHIP=""
CHANNEL=""

usage() { sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; }

while (( $# )); do
    case "$1" in
        -h|--help)       usage; exit 0 ;;
        --temp-cap)      TEMP_CAP=$2; shift 2 ;;
        --stop-secs)     STOP_SECS=$2; shift 2 ;;
        --spin-secs)     SPIN_SECS=$2; shift 2 ;;
        --force)         FORCE=1; shift ;;
        --no-gpu-check)  GPU_CHECK=0; shift ;;
        -*)              echo "unknown flag: $1" >&2; exit 2 ;;
        *)
            if   [[ -z $CHIP ]];    then CHIP=$1
            elif [[ -z $CHANNEL ]]; then CHANNEL=$1
            else echo "unexpected positional arg: $1" >&2; exit 2
            fi
            shift ;;
    esac
done

if [[ -z $CHIP || -z $CHANNEL ]]; then
    echo "usage: sudo $0 <chip-name> <pwm-channel> [options]" >&2
    echo "       run with --help for details" >&2
    exit 2
fi
if ! [[ $CHANNEL =~ ^[0-9]+$ ]]; then
    echo "pwm-channel must be a positive integer, got: $CHANNEL" >&2
    exit 2
fi

# ── preflight ────────────────────────────────────────────────────────────
# Daemon-conflict check (read-only, no root needed) before chip resolution
# so the user gets the clearest error first.
if systemctl is-active --quiet tesla_fan_control.service 2>/dev/null; then
    echo "error: tesla_fan_control.service is active — refusing to fight the daemon for /sys writes." >&2
    echo "       sudo systemctl stop tesla_fan_control" >&2
    exit 1
fi

# Resolve chip name → hwmon path. Pure /sys reads, no root required —
# we want bad chip-name input to fail fast without a sudo round-trip.
HWMON_PATH=""
shopt -s nullglob
for d in /sys/class/hwmon/hwmon*; do
    [[ -r $d/name ]] || continue
    if [[ "$(<"$d/name")" == "$CHIP" ]]; then
        if [[ -n $HWMON_PATH ]]; then
            echo "error: chip name '$CHIP' resolves to multiple hwmon entries:" >&2
            echo "         $HWMON_PATH" >&2
            echo "         $d" >&2
            echo "       pass an absolute /sys/class/hwmon/hwmonN path instead." >&2
            exit 1
        fi
        HWMON_PATH=$d
    fi
done
# Allow the user to also pass an absolute hwmonN path as the first arg.
if [[ -z $HWMON_PATH && -d $CHIP && -r $CHIP/name ]]; then
    HWMON_PATH=$CHIP
    CHIP=$(<"$HWMON_PATH/name")
fi
if [[ -z $HWMON_PATH ]]; then
    echo "error: chip '$CHIP' not found under /sys/class/hwmon" >&2
    echo >&2
    # Hint: did the user pass a driver name instead of a chip name? The
    # nct6775 kernel module handles nct6776 / nct6791 / nct6795 / nct6798 /
    # … so 'nct6775' as input is a common mistake. Same for it87 → it8628 /
    # it8772 / etc.
    driver_match=""
    chip_match=""
    for d in /sys/class/hwmon/hwmon*; do
        [[ -r $d/name ]] || continue
        chip_name=$(<"$d/name")
        drv=$(basename "$(readlink -f "$d/device/driver" 2>/dev/null || echo unknown)")
        if [[ $drv == "$CHIP" ]]; then
            driver_match=$drv
            chip_match=$chip_name
            break
        fi
    done
    if [[ -n $driver_match ]]; then
        echo "       hint: '$CHIP' is the kernel DRIVER name. The daemon matches" >&2
        echo "             against the CHIP name from /sys/class/hwmon/*/name." >&2
        echo "             try: sudo $0 $chip_match $CHANNEL" >&2
        echo >&2
    fi
    echo "       available chips:" >&2
    for d in /sys/class/hwmon/hwmon*; do
        [[ -r $d/name ]] || continue
        drv=$(basename "$(readlink -f "$d/device/driver" 2>/dev/null || echo unknown)")
        printf '         %-12s  driver=%-10s  (%s)\n' "$(<"$d/name")" "$drv" "$d" >&2
    done
    exit 1
fi

PWM_FILE="$HWMON_PATH/pwm$CHANNEL"
ENABLE_FILE="$HWMON_PATH/pwm${CHANNEL}_enable"
FAN_FILE="$HWMON_PATH/fan${CHANNEL}_input"

# Existence check (no root needed) → catches bad channel numbers before sudo.
if [[ ! -e $PWM_FILE ]]; then
    echo "error: $PWM_FILE does not exist — channel $CHANNEL not present on chip '$CHIP'." >&2
    echo "       available pwm channels on this chip:" >&2
    for f in "$HWMON_PATH"/pwm[0-9]*; do
        bn=$(basename "$f")
        [[ $bn =~ ^pwm[0-9]+$ ]] && echo "         ${bn#pwm}" >&2
    done
    exit 1
fi

# Now check for root — needed for the actual /sys writes below.
if [[ $EUID -ne 0 ]]; then
    echo "error: must run as root to write pwm$CHANNEL / pwm${CHANNEL}_enable" >&2
    echo "       try: sudo $0 $CHIP $CHANNEL" >&2
    exit 1
fi

if [[ ! -w $PWM_FILE ]]; then
    echo "error: $PWM_FILE not writable (despite running as root — unusual)" >&2
    exit 1
fi
if [[ ! -w $ENABLE_FILE ]]; then
    echo "error: $ENABLE_FILE not writable" >&2
    exit 1
fi

# Snapshot starting state for restoration.
ORIG_PWM=$(<"$PWM_FILE")
ORIG_ENABLE=$(<"$ENABLE_FILE")

if [[ $FORCE -eq 0 && $ORIG_ENABLE -ne 5 && $ORIG_ENABLE -ne 2 ]]; then
    echo "error: pwm${CHANNEL}_enable is currently $ORIG_ENABLE (not BIOS auto)." >&2
    echo "       another tool may already control this channel. Pass --force to override." >&2
    exit 1
fi

# GPU temperature precheck.
if [[ $GPU_CHECK -eq 1 ]] && command -v nvidia-smi >/dev/null 2>&1; then
    max_temp=$(nvidia-smi --query-gpu=temperature.gpu --format=csv,noheader,nounits 2>/dev/null \
                | sort -n | tail -1 || echo 0)
    if [[ -n $max_temp && $max_temp -ge $TEMP_CAP ]]; then
        echo "error: at least one GPU is at ${max_temp} °C (cap=${TEMP_CAP})." >&2
        echo "       refusing to stop a fan while a card is hot." >&2
        echo "       wait for cards to cool, raise --temp-cap, or pass --no-gpu-check." >&2
        exit 1
    fi
    echo "GPU temp precheck: max=${max_temp:-?} °C (cap=${TEMP_CAP})"
fi

# ── restore-on-exit trap ─────────────────────────────────────────────────
restore() {
    local rc=$?
    set +e
    echo
    echo "restoring pwm${CHANNEL}=${ORIG_PWM}, pwm${CHANNEL}_enable=${ORIG_ENABLE}…"
    echo "$ORIG_PWM"    > "$PWM_FILE"    2>/dev/null
    echo "$ORIG_ENABLE" > "$ENABLE_FILE" 2>/dev/null
    exit $rc
}
trap restore EXIT INT TERM

# Discover all tach channels on this chip — we poll every one during the
# sweep so we can detect when pwm{N} drives a fan whose tach is wired to
# fan{M}_input where M ≠ N. Common with Y-cables and certain board layouts.
TACH_CHANNELS=()
for f in "$HWMON_PATH"/fan[0-9]*_input; do
    bn=$(basename "$f")
    [[ $bn =~ ^fan([0-9]+)_input$ ]] || continue
    TACH_CHANNELS+=("${BASH_REMATCH[1]}")
done

read_all_tachs() {
    # echoes "ch=rpm ch=rpm …" for every fanN_input on this chip
    local ch line=""
    for ch in "${TACH_CHANNELS[@]}"; do
        local v="?"
        [[ -r "$HWMON_PATH/fan${ch}_input" ]] && v=$(<"$HWMON_PATH/fan${ch}_input")
        line+="fan${ch}=${v} "
    done
    echo "${line% }"
}

# Read the optional pwm_mode attribute (0 = DC voltage, 1 = PWM duty).
# Wrong mode is a common reason duty writes appear to do nothing.
MODE_FILE="$HWMON_PATH/pwm${CHANNEL}_mode"
PWM_MODE="?"
[[ -r $MODE_FILE ]] && PWM_MODE=$(<"$MODE_FILE")

# ── identify ─────────────────────────────────────────────────────────────
echo "──────────────────────────────────────────────────────────────"
echo "chip:        $CHIP"
echo "hwmon_path:  $HWMON_PATH"
echo "pwm_channel: $CHANNEL  (pwm_mode=$PWM_MODE: $(case "$PWM_MODE" in 0) echo "DC voltage";; 1) echo "PWM duty";; *) echo "unknown/n-a";; esac))"
echo "before:      pwm=$ORIG_PWM  pwm_enable=$ORIG_ENABLE"
echo "tach snapshot (all channels):"
echo "  $(read_all_tachs)"
echo "──────────────────────────────────────────────────────────────"
echo

read -r -p "Stop pwm${CHANNEL} for ${STOP_SECS}s, then spin to full for ${SPIN_SECS}s. Continue? [y/N] " yn
[[ $yn =~ ^[Yy]$ ]] || { echo "aborted."; exit 0; }

# Capture rpms across every tach BEFORE we touch anything, so we can later
# tell which channel responded to the duty change.
declare -A RPM_BEFORE
for ch in "${TACH_CHANNELS[@]}"; do
    [[ -r "$HWMON_PATH/fan${ch}_input" ]] && RPM_BEFORE[$ch]=$(<"$HWMON_PATH/fan${ch}_input") || RPM_BEFORE[$ch]=0
done

echo "→ taking manual control (pwm${CHANNEL}_enable = 1)"
echo 1 > "$ENABLE_FILE"

echo "→ stopping fan (pwm${CHANNEL} = 0)"
echo 0 > "$PWM_FILE"
for ((i=STOP_SECS; i>0; i--)); do
    printf '\r   stopped: %2ds  %s   ' "$i" "$(read_all_tachs)"
    sleep 1
done
echo

# Capture rpms while duty is held at 0 — the channel(s) that dropped are
# the real targets.
declare -A RPM_STOPPED
for ch in "${TACH_CHANNELS[@]}"; do
    [[ -r "$HWMON_PATH/fan${ch}_input" ]] && RPM_STOPPED[$ch]=$(<"$HWMON_PATH/fan${ch}_input") || RPM_STOPPED[$ch]=0
done

echo "→ spinning fan (pwm${CHANNEL} = 255)"
echo 255 > "$PWM_FILE"
for ((i=SPIN_SECS; i>0; i--)); do
    printf '\r   spun-up: %2ds  %s   ' "$i" "$(read_all_tachs)"
    sleep 1
done
echo
echo

# ── diagnosis ────────────────────────────────────────────────────────────
echo "──────────────────────────────────────────────────────────────"
echo "Diagnosis:"
echo

# A channel "responded" if its RPM dropped by >25% AND by >100 absolute rpm
# between BEFORE and STOPPED. Both thresholds avoid noise on near-stalled
# fans and on idle 0-rpm channels.
responders=()
for ch in "${TACH_CHANNELS[@]}"; do
    before=${RPM_BEFORE[$ch]}
    stopped=${RPM_STOPPED[$ch]}
    [[ $before -lt 200 ]] && continue   # skip channels that were idle / unwired
    drop=$((before - stopped))
    pct=$(( drop * 100 / (before > 0 ? before : 1) ))
    if (( drop > 100 && pct > 25 )); then
        responders+=("$ch")
        printf '  ✓ fan%s_input dropped %d → %d rpm (Δ%d, %d%%) — responded to pwm%s\n' \
            "$ch" "$before" "$stopped" "$drop" "$pct" "$CHANNEL"
    fi
done

target_responded=0
for ch in "${responders[@]}"; do
    [[ $ch == "$CHANNEL" ]] && target_responded=1
done

if [[ ${#responders[@]} -eq 0 ]]; then
    echo "  ✗ NO tach channel responded to pwm${CHANNEL} duty change."
    echo
    echo "  Most likely causes:"
    echo "    • The fan on pwm${CHANNEL} has no tach wire (or its tach landed on"
    echo "      a header whose fanN_input was already 0 before we started)."
    echo "    • pwm_mode mismatch — chip is in $([[ $PWM_MODE == 0 ]] && echo DC || echo PWM) mode"
    echo "      but the fan expects the other. Try:"
    echo "        echo $((1 - ${PWM_MODE:-0})) | sudo tee $MODE_FILE"
    echo "      then re-run this script. (Safe to revert.)"
    echo "    • BIOS / SuperIO has a minimum-duty floor enforced in hardware that"
    echo "      ignores writes below it (rare on nct6775, common on it87)."
    echo "    • The pwm${CHANNEL} header may not be physically populated."
elif [[ $target_responded -eq 1 && ${#responders[@]} -eq 1 ]]; then
    echo "  → pwm${CHANNEL} drives the fan reading on fan${CHANNEL}_input. Use:"
    echo "        chip        = $CHIP"
    echo "        pwm_channel = $CHANNEL"
else
    echo "  → pwm${CHANNEL} drives a fan whose TACH is wired to a different header."
    echo "    The tach you see in the daemon's logs will be on fan${responders[0]}_input,"
    echo "    not fan${CHANNEL}_input. The daemon assumes (pwm_channel) ↔ (fan_input)"
    echo "    use the same N, so this board layout will confuse fail-detection."
    echo
    echo "  Workarounds:"
    echo "    • If you can move cables: re-route the tach wire so it lands on"
    echo "      header ${CHANNEL} (matching pwm${CHANNEL})."
    echo "    • Otherwise, use pwm channel ${responders[0]} in the config (whichever"
    echo "      pwmN actually drives the fan you want — re-run this script with"
    echo "      that channel to confirm)."
fi
echo "──────────────────────────────────────────────────────────────"
echo
echo "Done. (trap will restore original pwm + pwm_enable on exit.)"
