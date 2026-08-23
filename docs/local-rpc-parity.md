# Local protocol-28 RPC parity runbook

Capture parity fixtures from a **local Stellar RPC pinned to protocol 28** — the
same ledger protocol our pinned host (`soroban-env-host` 28.0.2 → network
protocol 28) runs. This closes the raw-instruction / fee-metering residual that
the existing testnet fixtures carry, and unlocks deterministic parity against our
own custom-wasm contracts.

## Why this closes the residual

The committed fixtures under `fixtures/parity/` were captured from **public
Stellar testnet at ledger protocol 27**. `soroban-sim-core` links
`soroban-env-host` 28.0.2, whose network interface protocol is **28**. The host's
replay-safety guard (`check_ledger_protocol_supported`) rejects any ledger
protocol != 28, so the differential harness pins the simulated ledger context to
protocol 28 when replaying those p27 fixtures. RESULT / FOOTPRINT / AUTH / EVENT
parity is unaffected, but **raw CPU instruction metering and the resource fee
derived from it differ between protocol 27 and 28** — that cross-protocol delta
is exactly the `divergent-explained` residual (resource fee within ~0.1%) the
harness documents.

Capture the same transaction against a **protocol-28** RPC and both sides meter
under the *same* protocol, removing the cross-protocol component of the residual.

## Actual result (`local-add-i32`, captured 2026-08-23)

Captured against a local protocol-28 quickstart node (`stellar/quickstart:future`,
custom add-i32 wasm). Verdicts vs the recorded RPC `simulateTransaction`:

- **byte-equal / `match`:** result (`ScVal::I32(42)`), footprint (2 read-only,
  0 read-write), auth (0), contract events (0), read/write bytes — the first
  **custom-wasm** parity, not just the built-in SAC.
- **residual shrank but did not fully close:** raw instructions 244 730 (ours)
  vs 250 377 (RPC-implied) → +5 647; resource fee 50 231 vs 50 245 → **+14
  stroops (0.028%)**, versus ~0.1% for the protocol-27 testnet fixtures.

Root cause (grounded via `docker exec stellar-p28 stellar-core version`): the
quickstart core's protocol-28 host reports **`soroban-env-host` package version
28.0.0**, while this repo pins **`=28.0.2`**. The remaining delta is a
patch-level CPU-metering calibration difference (28.0.0 → 28.0.2), *not* a
protocol difference and *not* a flaw in the adjustment model (which reproduces
`max(raw+50000, raw×1.04)` exactly). To drive it to zero, either build/point at a
core embedding env-host 28.0.2 (try `-t nightly`, rebuilt daily) or pin this repo
to 28.0.0 to match the deployed network.

## Prerequisites

- **Docker daemon running.** Docker Desktop is installed; start it with
  `open -a Docker` and wait ~30–90 s (`docker ps` should succeed).
- **stellar CLI** (27.x): `stellar --version`.
- Repo deps installed: `pnpm install`.

## 1. Start the local protocol-28 RPC

`stellar container start` runs a quickstart node (Stellar core + RPC + Horizon +
friendbot) on port 8000. The `local` network supports `--protocol-version`:

```sh
stellar container start local --protocol-version 28 --name p28
```

This exposes:

| service   | URL                                  |
| --------- | ------------------------------------ |
| RPC       | `http://localhost:8000/rpc`          |
| friendbot | `http://localhost:8000/friendbot`    |

Friendbot is enabled by default in local mode. Fund any account with
`GET http://localhost:8000/friendbot?addr=<G...>`. Watch startup / tail logs:

```sh
stellar container logs p28
```

Wait until the RPC reports healthy (the log shows core catching up and RPC
serving). Quick check:

```sh
curl -s http://localhost:8000/rpc \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}'
```

The local quickstart network passphrase is the standalone default:
`Standalone Network ; February 2017`.

## 2. Register the network + a funded identity (stellar CLI)

```sh
stellar network add local \
  --rpc-url http://localhost:8000/rpc \
  --network-passphrase "Standalone Network ; February 2017"

# Generate + friendbot-fund a deployer identity on the local network.
stellar keys generate deployer --network local --fund
```

## 3. Build + deploy the add-i32 fixture contract

Build the wasm (writes `fixtures/contracts/add_i32.wasm`):

```sh
pnpm run build:fixture-contract
```

Deploy it and capture the resulting contract id:

```sh
stellar contract deploy \
  --wasm fixtures/contracts/add_i32.wasm \
  --source deployer \
  --network local
# → prints the deployed contract id: C...
```

Optional sanity invocation (`add(a,b)`):

```sh
stellar contract invoke --id <C...> --source deployer --network local \
  -- add --a 7 --b 35        # → 42
```

## 4. Capture the protocol-28 parity fixture

The capture script honors these env vars:

- `SOROBAN_RPC_URL` — the local RPC endpoint.
- `FRIENDBOT_URL` — friendbot base; the script appends `/?addr=<G...>` itself, so
  set it to `http://localhost:8000/friendbot` (no trailing `/?addr=`).
- `SOROBAN_ALLOW_HTTP=1` — required for plain-http `localhost` (the SDK's
  `rpc.Server` rejects non-TLS URLs otherwise).
- `SOROBAN_NETWORK=local-p28` — tags the fixture's `network` field.
- `SOROBAN_NETWORK_PASSPHRASE` — the local standalone passphrase (governs
  signing + networkId; the `local-add-i32` contract id comes from env, so it is
  passphrase-independent).
- `LOCAL_CONTRACT_ID=<C...>` — the deployed add-i32 contract id from step 3.

The one command (writes `fixtures/parity/local-add-i32.json`):

```sh
SOROBAN_RPC_URL=http://localhost:8000/rpc \
FRIENDBOT_URL=http://localhost:8000/friendbot \
SOROBAN_ALLOW_HTTP=1 \
SOROBAN_NETWORK=local-p28 \
SOROBAN_NETWORK_PASSPHRASE="Standalone Network ; February 2017" \
LOCAL_CONTRACT_ID=<C...> \
pnpm run capture:parity local-add-i32
```

The script funds a throwaway source account via friendbot, simulates
`add(7, 35) -> 42` against the local RPC, and snapshots the footprint entries +
all `ConfigSetting` entries so the fixture replays offline with zero needs-state
round trips. On success it logs `protocol=28`.

## 5. Rerun the differential harness

```sh
pnpm --filter @theahaco/soroban-sim test
```

The `native-sac-*` fixtures classify their p27↔p28 metering delta as
`divergent-explained`. The `local-add-i32` fixture is captured at protocol 28, so
its cross-protocol component is gone; its remaining sub-0.03% residual is the
env-host 28.0.0-vs-28.0.2 skew documented under "Actual result" above. It is
already wired into the `FIXTURES` array in
`packages/soroban-sim/src/differential/differential.test.ts` and passes.

## Stop the network

```sh
stellar container stop p28
```

The quickstart container is started with `--rm`, so stopping it removes it and
its ledger state. Re-running step 1 starts a fresh protocol-28 network (you will
need to redeploy the contract and recapture).
