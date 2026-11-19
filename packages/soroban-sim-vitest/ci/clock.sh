#!/usr/bin/env bash
# Times the CI steps the run summary reports. Marks live in $RUNNER_TEMP/clock.
#   clock.sh start LABEL | clock.sh stop LABEL   mark around steps that are actions
#   clock.sh run LABEL -- COMMAND...             time a command, keeping its exit status
#   clock.sh seconds LABEL                       stop - start, one decimal ("" if either is missing)
set -euo pipefail
dir="${RUNNER_TEMP:-/tmp}/clock"
mkdir -p "$dir"
now() { date +%s.%N; }

case "${1:-}" in
  start) now >"$dir/$2.start" ;;
  stop) now >"$dir/$2.stop" ;;
  run)
    label=$2
    shift 3
    now >"$dir/$label.start"
    status=0
    "$@" || status=$?
    now >"$dir/$label.stop"
    exit "$status"
    ;;
  seconds)
    if [ -f "$dir/$2.start" ] && [ -f "$dir/$2.stop" ]; then
      awk -v a="$(cat "$dir/$2.start")" -v b="$(cat "$dir/$2.stop")" 'BEGIN { printf "%.1f\n", b - a }'
    fi
    ;;
  *)
    echo "usage: clock.sh start|stop|seconds LABEL, or clock.sh run LABEL -- COMMAND..." >&2
    exit 2
    ;;
esac
