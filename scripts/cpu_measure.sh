#!/usr/bin/env bash
#
# cpu_measure.sh — sample per-process CPU usage of a native (or WebView) build.
#
# Usage:
#   ./scripts/cpu_measure.sh <pattern> [interval] [duration] [csv_out]
#
#   pattern   A bare name matches /proc/<pid>/exe basename (immune to the
#             15-char comm truncation and to this script's own subshells);
#             a pattern containing "/" is a raw /proc/<pid>/cmdline match.
#   interval  sampling period in seconds (default 0.5)
#   duration  how long to sample in seconds (default 30)
#   csv_out   optional CSV path (default /tmp/cpu_measure.csv)
#
# CPU% per sample = (utime+stime ticks delta / interval) / CLK_TCK * 100,
# aggregated across ALL threads of the process (sum of per-thread stat files),
# which is what matters for a GTK/Cairo redraw-heavy app.
#
# Prints mean / min / max / stddev over the samples on exit.  Exit status is
# nonzero if no matching process was found or no samples could be collected.
#
# Example:
#   ./scripts/cpu_measure.sh bspwm-cyberquote-native 0.5 30

set -u

name="${1:-}"
interval="${2:-0.5}"
duration="${3:-30}"
csv="${4:-/tmp/cpu_measure.csv}"

if [[ -z "$name" ]]; then
    echo "usage: $0 <pattern> [interval] [duration] [csv_out]" >&2
    exit 2
fi

if ! [[ "$interval" =~ ^[0-9]+(\.[0-9]+)?$ ]] || ! [[ "$duration" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
    echo "error: interval and duration must be positive numbers" >&2
    exit 2
fi

# --- process discovery ---
declare -A PIDS

if [[ "$name" == *"/"* ]]; then
    # raw cmdline match
    for f in /proc/[0-9]*/cmdline; do
        cmd="$(tr '\0' ' ' < "$f")"
        if [[ "$cmd" == *"$name"* ]]; then
            pid="${f%/cmdline}"
            PIDS["${pid##*/}"]=1
        fi
    done
else
    # basename of /proc/<pid>/exe
    for f in /proc/[0-9]*/exe; do
        [[ -e "$f" ]] || continue
        exe="$(readlink "$f" 2>/dev/null)" || continue
        base="${exe##*/}"
        if [[ "$base" == *"$name"* ]]; then
            pid="${f%/exe}"
            PIDS["${pid##*/}"]=1
        fi
    done
fi

if ((${#PIDS[@]} == 0)); then
    echo "error: no process matched '$name'" >&2
    exit 1
fi

# --- helpers ---
# sum of (utime + stime) ticks across all threads of a pid
tick_all() {
    local pid="$1" total=0 t
    local f
    for f in /proc/"$pid"/task/*/stat; do
        [[ -r "$f" ]] || continue
        # field 14 = utime, field 15 = stime; name in parens can contain spaces
        t="$(sed -n 's/^[^)]*) \([0-9]* [0-9]*\).*/\1/p' "$f")"
        [[ -z "$t" ]] && continue
        total=$((total + ${t% *} + ${t#* }))
    done
    echo "$total"
}

clk_tck="$(getconf CLK_TCK 2>/dev/null || echo 100)"

# --- sampling ---
: > "$csv"
echo "pid,cpu_pct" >> "$csv"

declare -A last    # pid -> ticks at previous sample

samples=0
start="$(date +%s.%N)"
end="$(awk -v s="$start" -v d="$duration" 'BEGIN { print s + d }')"

while :; do
    for pid in "${!PIDS[@]}"; do
        cur="$(tick_all "$pid")"
        if [[ -n "${last[$pid]:-}" ]]; then
            dt="$interval"
            ticks=$((cur - last[$pid]))
            pct="$(awk -v tk="$ticks" -v itv="$dt" -v clk="$clk_tck" \
                'BEGIN { printf "%.3f", (tk * 100.0) / (itv * clk) }')"
            echo "$pid,$pct" >> "$csv"
            ((samples += 1))
        fi
        last[$pid]="$cur"
    done
    now="$(date +%s.%N)"
    awk -v n="$now" -v e="$end" 'BEGIN { exit !(n < e) }' || break
    sleep "$interval"
done

if ((samples == 0)); then
    echo "error: no samples collected (interval too short or process died)" >&2
    exit 1
fi

# --- summary ---
awk -F, 'NR > 1 {
    s += $2; sum2 += $2 * $2;
    if ($2 < mn || NR == 2) mn = $2;
    if ($2 > mx || NR == 2) mx = $2;
    n++
}
END {
    mean = s / n;
    var = (sum2 / n) - (mean * mean);
    if (var < 0) var = 0;
    printf "samples=%d mean=%.3f%% min=%.3f%% max=%.3f%% stddev=%.3f%%\n", n, mean, mn, mx, sqrt(var);
    printf "csv: %s\n", "'"$csv"'";
}' "$csv"