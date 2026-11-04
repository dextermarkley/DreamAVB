#!/bin/bash
# Watches the DreamAVB daemon and the I210 dext and records WHY a stream stops.
# No agent, no tokens, no sudo, read-only: it only runs avb-probe info/counters,
# ioreg, pgrep and reads the daemon log.
#
# Usage:  scripts/monitor-dreamavb.sh [interval_seconds]      (default 5)
# Stop:   Ctrl-C
#
# Output (all under .agents/tmp/monitor/, gitignored):
#   status.csv   one row per tick: time, daemon pid, uptime, fault, link, attached,
#                tx_rate_limited, tx_batch_partial, rx_gptp, rx_avdecc, rx_stream
#   events.log   one entry per event (daemon restart, fault, link loss, dext
#                detach, error lines in the daemon log), each followed by a
#                snapshot: log tail, probe info and counters, ioreg, Thunderbolt
#                and thermal state.
#
# Environment:
#   MAX_TICKS=N   stop after N ticks (for testing)
#   NOTIFY=0      disable the terminal bell and macOS notification on events
#
# Notifications: if terminal-notifier is installed (brew install
# terminal-notifier), each event also posts a notification that opens
# events.log when clicked. Without it there is no notification by default,
# because macOS attributes osascript notifications to Script Editor and a
# click opens Script Editor; NOTIFY=osascript turns those on anyway.

ROOT="$(dirname "$(dirname "${BASH_SOURCE[0]}")")"
case "$ROOT" in
  /*) ;;
  *) ROOT="$PWD/$ROOT" ;;
esac
INTERVAL="${1:-5}"
OUT="$ROOT/.agents/tmp/monitor"
BIN="$ROOT/build/AVBInstaller.app/Contents/MacOS"
PROBE="$BIN/avb-probe"
DLOG="$HOME/Library/Logs/DreamAVB-daemon.log"
mkdir -p "$OUT"
export TMPDIR="$ROOT/.agents/tmp"

CSV="$OUT/status.csv"
EVENTS="$OUT/events.log"
[ -s "$CSV" ] || echo "time,daemon_pid,uptime_s,fault,link_up,dext_attached,tx_rate_limited,tx_batch_partial,rx_gptp,rx_avdecc,rx_stream" >"$CSV"

now() { date '+%Y-%m-%d %H:%M:%S'; }

# Value of a "name [ n] value" counter line from avb-probe counters.
counter() { printf '%s\n' "$1" | awk -v k="$2" '$1==k {print $NF; exit}'; }

snapshot() {
  {
    echo "---- daemon log tail ----"
    tail -n 40 "$DLOG" 2>/dev/null | grep -v -E "GET_STREAM_INFO|we transmit stream_id" | tail -n 25 | cut -c1-240
    echo "---- avb-probe info ----"
    timeout 10 "$PROBE" info 2>&1 | grep -E "fault|flags|STATUS|stage"
    echo "---- avb-probe counters (non-zero) ----"
    timeout 10 "$PROBE" counters 2>&1 | awk '$NF!=0'
    echo "---- ioreg AVBI210Driver ----"
    /usr/sbin/ioreg -rn AVBI210Driver -w0 2>&1 | grep -E "AVBI210Driver|com.avbaudio.installer.i210|IOProviderClass|registered|matched|busy" | head -8
    echo "---- Thunderbolt ----"
    /usr/sbin/system_profiler SPThunderboltDataType 2>&1 | head -60
    echo "---- thermal / power ----"
    pmset -g therm 2>&1 | head -6
    pmset -g batt 2>&1 | head -2
    echo "---- end snapshot ----"
  } >>"$EVENTS" 2>&1
}

event() {
  {
    echo
    echo "==== $(now)  EVENT: $1"
  } >>"$EVENTS"
  echo "$(now)  EVENT: $1"
  snapshot
  if [ "${NOTIFY:-1}" != "0" ]; then
    printf '\a'
    if command -v terminal-notifier >/dev/null 2>&1; then
      # Clicking this one opens the events log.
      terminal-notifier -title "DreamAVB monitor" -message "$1" \
        -open "file://$EVENTS" -group dreamavb-monitor >/dev/null 2>&1
    elif [ "${NOTIFY:-1}" = "osascript" ]; then
      # macOS attributes osascript notifications to Script Editor, so
      # clicking one opens Script Editor. Opt-in only for that reason.
      /usr/bin/osascript -e "display notification \"$1\" with title \"DreamAVB monitor\"" >/dev/null 2>&1
    fi
  fi
}

echo "DreamAVB monitor: every ${INTERVAL}s -> $CSV and $EVENTS   (Ctrl-C to stop)"

last_pid=""
last_fault="0"
last_link="true"
last_attached=1
daemon_start_epoch=0
log_lines=$(wc -l <"$DLOG" 2>/dev/null || echo 0)
log_lines=$((log_lines + 0))
ticks=0
echo "$(now)  monitor started" >>"$EVENTS"

while :; do
  pid="$(pgrep -f 'avb-daemon duplex' | head -n 1)"
  [ -z "$pid" ] && pid=0
  if [ "$pid" != "$last_pid" ]; then
    if [ -n "$last_pid" ]; then
      event "daemon pid changed $last_pid -> $pid (exit or restart)"
    fi
    daemon_start_epoch=$(date +%s)
    last_pid="$pid"
  fi
  uptime=$(($(date +%s) - daemon_start_epoch))

  info="$(timeout 10 "$PROBE" info 2>&1)"
  fault="$(printf '%s\n' "$info" | awk '/^stage/ {for (i=1;i<=NF;i++) if ($i=="fault") {print $(i+1); exit}}')"
  [ -z "$fault" ] && fault="?"
  link="$(printf '%s\n' "$info" | sed -n 's/.*link_up=\([a-z]*\).*/\1/p' | head -n 1)"
  [ -z "$link" ] && link="?"
  attached="$(/usr/sbin/ioreg -rn AVBI210Driver -w0 2>/dev/null | grep -c com.avbaudio.installer.i210)"

  cnt="$(timeout 10 "$PROBE" counters 2>&1)"
  echo "$(now),$pid,$uptime,$fault,$link,$attached,$(counter "$cnt" tx_rate_limited),$(counter "$cnt" tx_batch_partial),$(counter "$cnt" rx_gptp),$(counter "$cnt" rx_avdecc),$(counter "$cnt" rx_stream)" >>"$CSV"

  if [ "$fault" != "$last_fault" ] && [ "$fault" != "?" ]; then
    event "dext fault code changed $last_fault -> $fault (1 = register read all-ones, device gone or hung)"
    last_fault="$fault"
  fi
  if [ "$link" != "$last_link" ] && [ "$link" != "?" ]; then
    event "I210 link_up changed $last_link -> $link"
    last_link="$link"
  fi
  if [ "$attached" -lt 1 ] && [ "$last_attached" -ge 1 ]; then
    event "AVBI210Driver no longer attached in the kernel"
  fi
  last_attached="$attached"

  # New daemon log lines that matter.
  total=$(wc -l <"$DLOG" 2>/dev/null || echo 0)
  total=$((total + 0))
  if [ "$total" -lt "$log_lines" ]; then log_lines=0; fi
  if [ "$total" -gt "$log_lines" ]; then
    hits="$(sed -n "$((log_lines + 1)),${total}p" "$DLOG" | grep -E "error:|dext fault|device gone|left LOCKED|talker done|no AVDECC entity|exit [0-9]" | cut -c1-260 | head -n 6)"
    if [ -n "$hits" ]; then
      event "daemon log: $(printf '%s' "$hits" | head -n 1 | cut -c1-120)"
      printf '%s\n' "$hits" >>"$EVENTS"
    fi
    log_lines="$total"
  fi

  ticks=$((ticks + 1))
  if [ -n "$MAX_TICKS" ] && [ "$ticks" -ge "$MAX_TICKS" ]; then break; fi
  sleep "$INTERVAL"
done
echo "$(now)  monitor stopped" >>"$EVENTS"
