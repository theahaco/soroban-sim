#!/usr/bin/env bash
# Waits until a local network's RPC is healthy and its friendbot funds a new
# account, at most TIMEOUT seconds (default 300).
#   wait-for-funding.sh RPC_URL FRIENDBOT_URL [TIMEOUT]
set -euo pipefail
rpc=$1 friendbot=$2 timeout=${3:-300}
here=$(cd "$(dirname "$0")/.." && pwd)
deadline=$((SECONDS + timeout))
while [ "$SECONDS" -lt "$deadline" ]; do
  health=$(curl -s -X POST -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' "$rpc" || true)
  if echo "$health" | grep -q '"status":"healthy"'; then
    account=$(cd "$here" && node -e "import('@stellar/stellar-sdk').then((s) => console.log(s.Keypair.random().publicKey()))")
    code=$(curl -s -o /dev/null -w '%{http_code}' "$friendbot?addr=$account" || true)
    [ "$code" = 200 ] && exit 0
  fi
  sleep 2
done
echo "the local network could not fund an account within ${timeout}s" >&2
exit 1
