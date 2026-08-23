# Soroban Sim

Portable Soroban simulation spike for browsers, WASI Preview 2 components, and native Rust.

Target repository: `theahaco/soroban-sim`  
Intended npm package: `@theahaco/soroban-sim`

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

- `soroban-simulation = 28.0.2`
- `soroban-env-host = 28.0.2`
- `rs-soroban-env` tag `v28.0.2`, git commit `5061e9c40ce6fc74ade9b9b3b49465b0cf7fdccb`
- `soroban-wasmi = 0.31.1-soroban.20.0.1` through `soroban-env-host`
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
  landed and demonstrates parity against a live Stellar testnet
  `simulateTransaction` over two fixtures: the native SAC `decimals()`
  (read-only) and a native SAC `transfer()` (write + auth + contract event). On
  both, the invocation result, footprint (read-only **and** read-write), auth
  entries, and contract events are byte-identical to RPC. An `AdjustmentSource`
  knob (`apply-rpc-adjustments`) reproduces stellar-rpc's resource/fee adjustment
  model exactly (RpcDefault instructions = raw metered + 50000 on both); the only
  residual is a small raw-CPU-metering delta from the captured fixtures being
  protocol 27 vs the host's protocol 28, classified `divergent-explained`.
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
sha256=1082a3e46e6b702d61c740fd9151067a65054d63bc63f9bda6535de1c045263f
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
  instructions = raw + 50000 on both fixtures. A small residual remains in the
  *raw* metered numbers because the fixtures were captured at protocol 27 while
  the host runs protocol 28 — a metering delta, not an adjustment difference.
- No authorization-enforcement API (recording-mode auth is captured but not
  enforced against a supplied auth tree) yet.

Now implemented (previously listed as gaps): full network-config encoding
(explicit + `FromLedgerEntries`), missing-state / present-absent-unknown state
with `needs-state` reruns, the RPC state provider, IndexedDB caching,
transaction-envelope decoding, and cancellation / budget limits.

## Docs

- `docs/architecture.md` — design and state-loading plan.
- `docs/feasibility.md` — Milestone 0 proof and blocker inventory.
- `docs/differential-testing.md` — oracles A/B/C and the M3 RPC-equivalence result.
- `docs/upstream.md` — upstream pins and contribution notes.

## License

Apache-2.0.
