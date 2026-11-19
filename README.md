# Soroban Sim

Portable Soroban simulation spike for browsers, WASI Preview 2 components, and native Rust.

## Why rehearse on a fork

Testnet is shared, and its costs can move away from mainnet's. On 2026-10-25,
testnet's live Soroban state (about 4.2 GB) had grown past the network's 4 GB
state target. Above the target, rent rises steeply. Writing one new persistent
entry and keeping it for the maximum TTL simulated at 400–510 XLM, against under
1 XLM five days earlier. Uploading a 7 KB contract cost about 2,000 XLM, more
than a plain transaction's fee can be. A network configuration upgrade at ledger
5,385,212 (17:34:07 UTC that day) raised the target to 7 GB, and costs fell back.
Mainnet held about 1.7 GB against a 3 GB target throughout.

Soroban Sim loads the network configuration from the forked ledger's own
`ConfigSetting` entries. A session forked from mainnet
([network sessions](#network-sessions), `soroban-sim serve --fork mainnet`)
therefore prices fees and rent as mainnet would, runs on your own machine, and
adds no load to testnet.

## Installation and releases

The command-line tool is the `soroban-sim-cli` package; its executable is
`soroban-sim`. Version 0.1.0 is published on
[crates.io](https://crates.io/crates/soroban-sim-cli) and as prebuilt binaries on
the [GitHub release](https://github.com/theahaco/soroban-sim/releases/tag/soroban-sim-cli-v0.1.0).
Install it one of three ways:

```sh
cargo install soroban-sim-cli --locked   # build from source; needs Rust 1.89+
cargo binstall soroban-sim-cli           # prebuilt binary; needs cargo-binstall
soroban-sim --version
```

or download a prebuilt archive yourself. Each release has one archive per
target, named `soroban-sim-cli-v<version>-<target>.tar.gz` (Windows also gets a
`.zip`), holding a single `soroban-sim` (`soroban-sim.exe` on Windows), and a
`.sha256` checksum file beside it:

| OS | Targets |
|---|---|
| Linux | `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl` |
| macOS | `x86_64-apple-darwin`, `aarch64-apple-darwin` |
| Windows | `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` |
| FreeBSD | `x86_64-unknown-freebsd` |

CI runs `--version` and `--help` from every archive built on a native runner. The
cross-built Windows arm64, Linux musl arm64 and FreeBSD archives are checked for
layout and checksum only.

```sh
tag=soroban-sim-cli-v0.1.0
target=x86_64-unknown-linux-gnu
base=https://github.com/theahaco/soroban-sim/releases/download/$tag
curl -LO "$base/$tag-$target.tar.gz" -LO "$base/$tag-$target.sha256"
sha256sum -c "$tag-$target.sha256"   # macOS: shasum -a 256 -c
tar -xzf "$tag-$target.tar.gz"
./soroban-sim --version
```

On Windows, download the `.zip` and compare `Get-FileHash <archive>` in
PowerShell with the matching line of the `.sha256` file.

The Rust libraries are `soroban-sim-core`, `soroban-sim-facade` and
`soroban-sim-component` (0.1.0). `soroban-sim-core-p30`, `soroban-sim-facade-p30`
and `soroban-sim-cli-p30` are generated copies of core, the facade and the CLI's
library against the protocol-30 host, so the one `soroban-sim` binary runs both
protocols. `soroban-sim-host` (29.0.2) is the isolated
upstream application host used for exact native metering; its
[`UPSTREAM.json`](crates/soroban-sim-host/UPSTREAM.json) records which
`soroban-env-host` release it copies. The TypeScript package
`@theahaco/soroban-sim` (`packages/soroban-sim`) is not published to npm yet.
See [Publishing and release maintenance](docs/releases.md) for how releases are
cut, verified and recovered.

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
- `soroban-env-host = 29.0.2` (protocol 29, Mainnet's)
- `stellar-xdr = 28.0.0` through upstream crates
- the CLI also links `soroban-env-host`, `soroban-simulation` and
  `stellar-xdr` 30.0.0 (protocol 30, Testnet's) through the `-p30` crates
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
ever submitted; throwaway keys stand in for every real signer.

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

Snapshot files fork offline. `session run --save-snapshot` and `serve
--save-snapshot` keep the base entries a session read (format
`soroban-sim-snapshot/1`). `fork_exportSnapshot` returns the session's whole
state (format `soroban-sim-snapshot/2`): a server started from it with `serve --snapshot`
answers the same reads and applies the next transaction the same way, so a
setup can run once and each test start from its result on its own server. An
export of a live fork is partial and still reads the keys it never read from
that network; an export of a snapshot base is self-contained. `soroban-sim
snapshot capture --network URL --out FILE` captures a network's settings and
latest ledger header as a self-contained snapshot. See
[`docs/rpc-facade.md`, "Snapshots"](docs/rpc-facade.md#snapshots).

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

## Sign dialog

`packages/soroban-sign-dialog` (`@theahaco/soroban-sign-dialog`) is an
embeddable `<soroban-sign-dialog>` web component for wallets and dapps. It
renders the full tree of requested authority from a transaction's XDR alone
(about 17 KB brotli, any protocol) under a plain summary, and lazily loads a
preview engine that re-records the tree with soroban-sim's host. The user sees
one status (will succeed, will be refused or unknown), a warning for entries
the transaction never uses, and, behind Details, balance changes, events and a
smart account's own verdict. The engine ships one reactor per host protocol
(`crates/soroban-sim-preview`, p29 and p30, about 470 KB brotli each) and
picks one by the network's `protocolVersion`. See the package README.

```sh
pnpm --filter @theahaco/soroban-sign-dialog build
pnpm --filter @theahaco/soroban-sign-dialog demo   # http://127.0.0.1:4178/demo/
```

## Test suites

`packages/soroban-sim-vitest` (`@theahaco/soroban-sim-vitest`) runs Vitest
suites whose setup runs once on `soroban-sim serve`. The result is exported
as a snapshot (`fork_exportSnapshot`) and cached on disk, and each test gets
its own server started from it, so tests are isolated and run in parallel.
The same suite also runs against a local network (quickstart), where setup
runs once per test with fresh friendbot-funded accounts. Which one runs is
the `SOROBAN_TEST_ENV` variable, not the test code. Contracts are the Stellar
SDK's own contract clients (`contract.Client`, or the class generated
bindings define) bound to funded accounts: `await (await token.transfer({
from: alice.address, to: bob.address, amount: 10n })).signAndSend()`, with
failures thrown as typed errors carrying the contract's error code. On
soroban-sim, tests can also set balances, patch contract storage and move
ledger time. See the package README for the API and for what differs between
the two environments.

```sh
cargo build --release -p soroban-sim-cli
pnpm --filter @theahaco/soroban-sim-vitest test         # soroban-sim
pnpm --filter @theahaco/soroban-sim-vitest test:local   # a running local network
```

## Docs

- `docs/architecture.md` — design and state-loading plan.
- `docs/session.md` — network sessions: engine, plan format, CLI, report.
- `docs/rpc-facade.md` — `soroban-sim serve`: the RPC facade, its `fork_*` methods, and rehearsing existing scripts.
- `docs/replay.md` — `soroban-sim replay`: replaying real network transactions to check the session's fidelity.
- `docs/browser-session.md` — network sessions in the browser: the WIT resource, the IndexedDB log, the virtual RPC, the timeline and replay safety.
- `docs/feasibility.md` — Milestone 0 proof and blocker inventory.
- `docs/differential-testing.md` — oracles A/B/C and the M3 RPC-equivalence result.
- `docs/upstream.md` — upstream pins and contribution notes.
- `packages/soroban-sign-dialog/README.md` — the sign dialog: embedding, frameworks, theming, the preview engine.
- `packages/soroban-sim-vitest/README.md` — Vitest suites on soroban-sim or a local network: workers, accounts and SDK contract clients, setup, sandbox controls, the cache, what differs.
- `CONTRIBUTING.md` — PR and commit templates, and the checks that run on every PR.

## License

Apache-2.0.
