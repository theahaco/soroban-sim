#!/usr/bin/env bash
# Writes one job's cost table to the run summary ($GITHUB_STEP_SUMMARY) and
# to the log, from clock.sh marks (the job stops its own clock, `clock.sh stop
# job`, once its timed steps are done) and the memory figures.
#   summary.sh soroban-sim   (reads BUILD_SECONDS, CLI_JOB_SECONDS, $RUNNER_TEMP/servers.rss and servers-cached.rss)
#   summary.sh quickstart    (reads QUICKSTART_MEMORY_PEAK_MIB)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
clock() { "$here/clock.sh" seconds "$1"; }
sum() { awk -v a="${1:-0}" -v b="${2:-0}" -v c="${3:-0}" 'BEGIN { printf "%.1f", a + b + c }'; }
minus() { awk -v a="${1:-0}" -v b="${2:-0}" 'BEGIN { printf "%.1f", a - b }'; }
or_na() { if [ -n "${1:-}" ]; then echo "$1"; else echo "n/a"; fi; }
rss() { if [ -f "$1" ]; then awk '{ printf "%s MiB (%s servers at once)", $1, $2 }' "$1"; else echo "n/a"; fi; }

job=$(clock job)

case "$1" in
  soroban-sim)
    binary=$(clock binary) first=$(clock first) cached=$(clock cached)
    tests=$(sum "$first" "$cached")
    from_source=$(sum "$(or_na "${CLI_JOB_SECONDS:-}")" "$(minus "$job" "$binary")")
    [ -n "${CLI_JOB_SECONDS:-}" ] || from_source="n/a"
    table=$(cat <<TABLE
### soroban-sim

| | Prebuilt binary (this job) | Built from source |
|---|---|---|
| Setup: get the CLI | ${binary} s, download the build artifact | $(or_na "${BUILD_SECONDS:-}") s \`cargo build --release\` (job \`vitest-cli\`, $(or_na "${CLI_JOB_SECONDS:-}") s in all) |
| Tests, first run (setup runs once, snapshot cached) | ${first} s | ${first} s |
| Tests again, setup from the cache | ${cached} s | ${cached} s |
| Tests in all | ${tests} s | ${tests} s |
| Total | ${job} s | ${from_source} s |
| Peak memory, soroban-sim servers | first run $(rss "${RUNNER_TEMP:-/tmp}/servers.rss"); cached run $(rss "${RUNNER_TEMP:-/tmp}/servers-cached.rss") | same |

- The latest release (soroban-sim-cli-v0.1.0) predates \`fork_exportSnapshot\`, which the harness needs, so the CLI is built once from this commit in \`vitest-cli\` (with the Rust cache restored) and downloaded here; "Built from source" adds that job's time and leaves out the download.
- Times are wall-clock between marks in the job; Total runs from the job's first step to the end of its last timed step, so it includes checkout, Node and \`pnpm install\` but not runner start-up, and nothing that runs after the tests.
- Memory is the summed resident memory of every \`soroban-sim\` server process, sampled with \`ps\` in a loop with no pause (a peak between two samples is missed, so this is a lower bound); the Node test runner is not counted. The sampler keeps a core busy, so it runs in two further runs of the suites (the cache cleared, then from the cache) after the clock has stopped: the timed runs above carry no sampler, and the memory runs are in none of the times, Total included.
TABLE
)
    ;;
  quickstart)
    start=$(clock container) ready=$(clock ready) tests=$(clock tests) since=$(clock since-start)
    setup=$(sum "$start" "$ready")
    if [ -n "${QUICKSTART_MEMORY_PEAK_MIB:-}" ]; then memory="${QUICKSTART_MEMORY_PEAK_MIB} MiB"; else memory="n/a (the container's cgroup memory.peak was not readable)"; fi
    table=$(cat <<TABLE
### Local network (quickstart)

| | Seconds |
|---|---|
| Setup: pull and start the container | ${start} s |
| Setup: wait until RPC is healthy and friendbot funds an account | ${ready} s ($(or_na "$since") s from \`docker run\`, overlapping the install) |
| Setup in all | ${setup} s |
| Tests | ${tests} s |
| Total | ${job} s |
| Peak memory, the container | ${memory} |

- Times are wall-clock between marks in the job; Total runs from the job's first step to the end of its last timed step, so it includes checkout, Node and \`pnpm install\` but not runner start-up, and nothing that runs after the tests.
- Memory is the container's cgroup \`memory.peak\`: the most it held at once since it started, page cache charged to it included. The kernel keeps that figure and it is read once after the tests, so nothing samples while they are timed.
TABLE
)
    ;;
  *)
    echo "usage: summary.sh soroban-sim|quickstart" >&2
    exit 2
    ;;
esac

echo "$table"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then echo "$table" >>"$GITHUB_STEP_SUMMARY"; fi
