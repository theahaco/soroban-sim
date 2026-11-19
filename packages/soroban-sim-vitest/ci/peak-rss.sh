#!/usr/bin/env bash
# Runs COMMAND while sampling the resident memory of every process named NAME
# as fast as `ps` returns (no pause between samples), and writes
# "<peak summed RSS in MiB> <most processes at once>" to OUT. The figures are
# a lower bound: a peak between two samples is missed.
#   peak-rss.sh NAME OUT -- COMMAND...
set -euo pipefail
name=$1 out=$2
shift 3

"$@" &
command=$!
while kill -0 "$command" 2>/dev/null; do
  ps -A -o rss=,ucomm= || true
  echo end
done | awk -v name="$name" '
  $0 == "end" { if (kib > peak) peak = kib; if (count > most) most = count; kib = count = 0; next }
  NF == 2 && $2 == name { kib += $1; count++ }
  END { printf "%.1f %d\n", peak / 1024, most }
' >"$out"
status=0
wait "$command" || status=$?
exit "$status"
