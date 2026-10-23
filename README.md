# Soroban Sim

Portable Soroban simulation spike for browsers, WASI Preview 2 components, and native Rust.

Target repository: `theahaco/soroban-sim`  
Intended npm package: `@theahaco/soroban-sim`

## Installation and releases

The native CLI package is `soroban-sim-cli`; its executable is `soroban-sim`.
Once the first release is published:

```sh
cargo install soroban-sim-cli --locked
cargo binstall soroban-sim-cli # prebuilt platform archive
soroban-sim --help
```

The Rust libraries are `soroban-sim-core`, `soroban-sim-facade` and
`soroban-sim-component`. `soroban-sim-host` is the isolated upstream application
host used for exact native metering. Rust 1.89+ is required for the locked
dependency graph. See [Publishing and release maintenance](docs/releases.md)
for first-release setup, package verification and recovery.

## Architecture

Soroban Sim puts the WebAssembly Component Model around the simulator, not around each Soroban contract:

```text
Browser UI
  -> TypeScript API + Web Worker
  -> Jco-transpiled WASI Preview 2 component
  -> soroban-simulation / soroban-env-host
  -> soroban-wasmi
  -> normal wasm32v1-none Soroban contract bytes
```

Contracts remain ordinary Soroban core Wasm modules. The embedded Soroban host loads them through Wasmi.

## Milestone 0 status

Implemented spike:

- Rust workspace with `soroban-sim-core` and `soroban-sim-component`.
- WIT world in `wit/simulator.wit`.
- Fixture contract source in `fixtures/contracts/add-i32`.
- Fixture wasm copied to `fixtures/contracts/add_i32.wasm`.
- Native fixture simulation through `soroban-simulation`.
- `wasm32-wasip2` component build.
- Wasmtime CLI fixture execution.
- Jco transpilation.
- Browser Worker execution via Playwright + Vite.
- Native, Wasmtime, and browser outputs matched golden SHA-256.

Pinned upstream:

- `soroban-simulation = 29.0.2`
- `soroban-env-host = 29.0.2` (protocol 29, matching Testnet and Mainnet)
- `stellar-xdr = 28.0.0` through upstream crates
- `soroban-wasmi = 29.0.0` through `soroban-env-host`
- fixture contract uses `soroban-sdk = 27.0.5`

## Milestone status

- **M1 (three-state snapshot + needs-state rerun) — done.** Present / absent /
  unknown state, the fail-closed `needs-state` protocol, and the byte-stable SSIM
  encoding are implemented and covered by `cargo test -p soroban-sim-core`.
- **M2 foundation — done.** Explicit and `FromLedgerEntries` network config,
  RPC state provider (`packages/soroban-sim/src/providers/rpc.ts`), IndexedDB
  caching, transaction-envelope decoding, and the `SorobanSimulator` rerun loop
  are in place.
- **M3 (differential testing) — in progress.** Oracles A, B, and C are green (see
  `docs/differential-testing.md`). The RPC-equivalence differential harness has
  landed and demonstrates parity against recorded protocol-29 RPC
  `simulateTransaction` responses over three fixtures: the native SAC
  `decimals()` (read-only) and a native SAC `transfer()` (write + auth + contract
  event) from Testnet, plus a custom-wasm `add()` from a local quickstart node.
  On all three, the invocation result, footprint (read-only **and** read-write),
  auth entries, and contract events are byte-identical to RPC. An
  `AdjustmentSource` knob (`apply-rpc-adjustments`) reproduces stellar-rpc's
  resource/fee adjustment model exactly (RpcDefault instructions = raw metered +
  50000 on all three), and instructions and resource fee match RPC exactly at
  protocol 29; any future residual is classified `divergent-explained`.
  Re-capture with `pnpm run capture:parity <case>`.

## Requirements

- Rust stable with targets `wasm32-wasip2` and `wasm32v1-none`
- Node 24+
- pnpm 11+
- Wasmtime CLI 48+
- Playwright Chromium for browser proof

```sh
rustup target add wasm32-wasip2 wasm32v1-none
brew install wasmtime # macOS; or install Wasmtime another way
pnpm install
pnpm --filter @theahaco/soroban-sim-web-demo exec playwright install chromium
```

## Milestone 0 commands

```sh
pnpm run build:fixture-contract
pnpm run test:native
pnpm run build:component
pnpm run build:jco
pnpm run test:ts
pnpm run test:wasmtime
pnpm run check:ts
pnpm run test:browser
```

All-in-one:

```sh
pnpm run m0
```

Golden fixture:

```text
len=388
sha256=3026ccbc8085dc49520836c688a504a93eb93b8206213d6fa8ef6a1a280e514c
```

## Measured M0 numbers

Measured on macOS arm64, Rust 1.97.1, Wasmtime 48.0.0, Chromium 151.

- component binary: `2.9 MiB`
- Jco core wasm: `2.88 MiB`
- Jco JS glue: `203 KiB`
- Wasmtime `run-fixture()`: `0.05s real`, `34.7 MB` max RSS, `29.3 MB` peak memory footprint
- Browser direct Jco import: `42.3 ms` startup, `11.0 ms` execution, about `2.7 MB` JS heap delta
- Browser Worker demo: `47 ms` worker startup, `10.7 ms` fixture execution

## Current limitations

- The component imports WASI Preview 2 CLI/io/clock/random shims because Rust `std` and upstream dependencies currently pull them in. It does not import WASI filesystem or sockets, and performs no network or persistent-filesystem access.
- RPC-equivalence authority is demonstrated for two captured native-SAC cases —
  a read-only `decimals()` and a write+auth+event `transfer()` — not yet across
  arbitrary custom-wasm contracts or state-archival/restore paths. See
  `docs/differential-testing.md` ("RPC equivalence (M3)") for scope.
- Resource and fee estimates default to the host's raw metered cost
  (`SimulationAdjustmentConfig::no_adjustments()`). Setting
  `apply-rpc-adjustments` (`AdjustmentSource::RpcDefault`) reproduces
  stellar-rpc's adjustment model exactly; the parity harness confirms adjusted
  instructions = raw + 50000 on every fixture. All fixtures are captured at
  protocol 29, the live-workbench host's protocol (native-SAC from Testnet,
  custom-wasm `local-add-i32` from a local quickstart node), and match RPC's
  instructions and resource fee exactly.
- The single-call simulation API records auth rather than enforcing it against
  a supplied auth tree. The network-session engine (below) enforces and
  verifies every envelope and auth signature.

Now implemented (previously listed as gaps): full network-config encoding
(explicit + `FromLedgerEntries`), missing-state / present-absent-unknown state
with `needs-state` reruns, the RPC state provider, IndexedDB caching,
transaction-envelope decoding, and cancellation / budget limits.

## Network sessions

`soroban-sim-core::session` rehearses many signed transactions in order over a
fork of a real network, and the `soroban-sim` CLI (`crates/soroban-sim-cli`)
runs a plan file through it and writes a JSON and a Markdown report. Nothing is
ever submitted; throwaway keys stand in for every real signer. Besides contract
calls, a session applies the classic operations a setup runs around them
(`CreateAccount`, `Payment`, `SetOptions`, `ChangeTrust`) and can close a
ledger with several transactions, as stellar-core does.

```sh
cargo run -p soroban-sim-cli -- session run examples/session/perch-nido-bootstrap.yaml \
  --wasm-dir path/to/release-wasm --differential
```

See `docs/session.md` for the design, the plan format, the signer adapters
(classic, OZ `oz_rule_ids`/`addr_oz_v1`, CAP-71 delegates, secp256r1, WebAuthn)
and what the rehearsal does and does not model.

`soroban-sim serve` puts the same session behind a Stellar RPC facade, so stock
`stellar`, stellar-sdk and the scripts built on them rehearse against a fork
unchanged, with `fork_*` methods for signer substitution, balances, the clock,
snapshots and the report:

```sh
cargo run -p soroban-sim-cli -- serve --fork mainnet --report rehearsal
export STELLAR_RPC_URL=http://127.0.0.1:8000
export STELLAR_NETWORK_PASSPHRASE="Public Global Stellar Network ; September 2015"
STELLAR_SIGN_WITH_KEY=<throwaway> ./your-deploy-script.sh
```

See `docs/rpc-facade.md`.

Both read a fork either lazily at RPC's latest ledger (`--fork-mode head`, the
default) or pinned to one ledger with every read rolled back to it through the
later ledgers' close meta (`--fork-mode checkpoint [--fork-ledger N]`).

`soroban-sim replay` checks the session's transaction layer against the
network: it records recent real mainnet and testnet Soroban transactions with
the exact state each applied against, replays them through the session, and
compares results, return values, events, after-images, fees and core's
resource metrics. The committed sample in `fixtures/replay` replays exactly in
CI; see `docs/replay.md`.

```sh
cargo run --release -p soroban-sim-cli -- replay fetch --network mainnet --min-ledgers 55 --out mainnet.json.gz
cargo run --release -p soroban-sim-cli -- replay run fixtures/replay/*.json.gz --report fidelity
```

The web demo runs the same session in the browser. The component exports it as
the WIT `network-session` resource, and its log and fork cache live in
IndexedDB, so a reload resumes it. A service worker serves it at a virtual RPC
URL, so stellar-sdk and the demo's wallet pre-submit flow submit into the fork
unchanged. The panel shows a step timeline with each step's fees, auth and
ledger changes, opens any step in the time-travel debugger, and ends with the
report. Signing defaults to throwaway keys, and a real wallet signing into a
session gets a replay warning. `soroban-sim serve` and the browser share one
request handler, `crates/soroban-sim-facade`. See `docs/browser-session.md`.

## Docs

- `docs/architecture.md` — design and state-loading plan.
- `docs/session.md` — network sessions: engine, plan format, CLI, report.
- `docs/rpc-facade.md` — `soroban-sim serve`: the RPC facade, its `fork_*` methods, and rehearsing existing scripts.
- `docs/replay.md` — `soroban-sim replay`: replaying real network transactions to check the session's fidelity.
- `docs/browser-session.md` — network sessions in the browser: the WIT resource, the IndexedDB log, the virtual RPC, the timeline and replay safety.
- `docs/feasibility.md` — Milestone 0 proof and blocker inventory.
- `docs/differential-testing.md` — oracles A/B/C and the M3 RPC-equivalence result.
- `docs/upstream.md` — upstream pins and contribution notes.

## License

Apache-2.0.
