# Upstream notes

## Pins

Soroban Sim M0 pins:

- `rs-soroban-env` tag `v28.0.2`
- tag commit `5061e9c40ce6fc74ade9b9b3b49465b0cf7fdccb`
- `soroban-env-host = 28.0.2`
- `soroban-simulation = 28.0.2`
- `stellar-xdr = 28.0.0` through upstream crates
- `soroban-wasmi = 0.31.1-soroban.20.0.1`

Snapshot-source prior art inspected:

- fork-testing design: `stellar/rs-soroban-sdk#1448`
- custom snapshot-source support: `stellar/rs-soroban-sdk#1620`, commit `37badee96c252ab5a3b90a862f3dd2842668a751`
- transaction-granular snapshot source: `stellar/rs-soroban-sdk#1657`, branch/commit `b91d50521250c67b0a66898cba6654401baa8ea6`

## Snapshot-source draft observations

Useful design:

- reconstruct state immediately before a transaction or after a ledger;
- search transaction metadata, earlier transaction metadata, RPC when safe, earlier ledger metadata, and history archives;
- cache entries by ledger / transaction context / ledger-key hash;
- invalidate caches by XDR schema version.

Native assumptions to keep out of portable core:

- `cargo_metadata` for workspace root;
- `directories` system cache paths;
- filesystem JSON fixtures;
- filesystem locks;
- blocking `reqwest`;
- `flate2` / `zstd` archive decompression inside snapshot source;
- tracing subscriber initialization from environment;
- synthetic `u32::MAX` TTL for contract entries.

## Proven M1 three-state mechanism

This section supersedes the earlier "local adapter plan" sketch. The mechanism
below is grounded in `soroban-simulation` / `soroban-env-host` 28.0.2 source,
with citations, and is the design that M1 implements.

### `SnapshotSource::get` returns one of three states

`SnapshotSource::get(&self, key: &Rc<LedgerKey>)` maps our tri-state cache onto
the host's `Result<Option<(Rc<LedgerEntry>, Option<u32>)>, HostError>` contract:

1. **Present** -> `Ok(Some((Rc<LedgerEntry>, Some(live_until_ledger))))`.
2. **Absent** -> `Ok(None)`.
3. **Unknown** -> push the `LedgerKey` into a `RefCell` recorded-keys buffer
   **and** return the sentinel `HostError`
   `(ScErrorType::Storage, ScErrorCode::ExceededLimit)`.

```rust
enum CachedEntry {
    Present { entry: LedgerEntry, live_until_ledger: Option<u32> },
    Absent,
    Unknown,
}
```

### The recorded-keys buffer is authoritative for NeedsState

The buffer, not the `HostError`, is the needs-state signal. Only our `get()`
writes it, so it is immune to host-side error decoration, `try_call`
absorption, and the `Result`-plumbing subtleties below.

After the call, classify on the buffer:

- `probe.take_needed()` non-empty -> `NeedsState(keys)`.
- otherwise -> `Complete` (encode the result, including a genuine contract
  failure inline as a `Complete` outcome).

### Why the host error does NOT surface as the outer `Result::Err`

`simulate_invoke_host_function_op` returns
`Ok(InvokeHostFunctionSimulationResult { invoke_result: Err(sentinel), .. })`,
**not** `Err(sentinel)` at the outer level
(`soroban-simulation/src/simulation.rs:146-168`). The host's invocation error
is captured into the `invoke_result` field during the post-invocation footprint
re-scan (`soroban-env-host/src/e2e_invoke.rs:870-872`). Consequently:

- **Do not** branch on the outer `Ok`/`Err` of the simulation call.
- **Do** branch on the recorded-keys buffer.

A get() `Err` therefore comes back as `Ok(result)` with `invoke_result = Err`,
and a real contract trap is indistinguishable at the outer `Result` level from
our Unknown sentinel — which is exactly why the buffer is the source of truth.

### Why `(Storage, ExceededLimit)` is a safe sentinel

- **Non-recoverable.** `is_recoverable`
  (`soroban-env-host/src/host/error.rs:148-167`) classifies
  `(Storage, ExceededLimit)` as non-recoverable, so a contract's `try_call`
  cannot catch and mask it mid-invocation.
- **Never emitted by the host in recording mode.** Its only in-host producer is
  `Footprint::enforce_access`, which runs in `AccessType::Enforcing` mode only;
  simulation runs in recording mode, so the host never emits this exact
  type/code itself. Any occurrence during simulation is ours.
- **Distinct from neighbouring limit errors.** Budget exhaustion surfaces as
  `(Budget, ExceededLimit)` and XDR depth/size limits as
  `(Context, ExceededLimit)`, so a genuine resource failure is classified
  `Complete`, never a false `NeedsState`.

Match with `e.error.is_type(ScErrorType::Storage) && e.error.is_code(ScErrorCode::ExceededLimit)`
as a debug assertion only — never by string. The buffer remains authoritative.

### Present contract entries MUST carry a real `live_until`

For `ContractData` / `ContractCode` entries, `get()` must return
`Some(live_until_ledger)` with a real `u32`. If the TTL is `None`, the host
aborts with `(ScErrorType::Storage, ScErrorCode::InternalError)`
(`soroban-env-host/src/storage.rs:819-826`). Do not synthesize `u32::MAX`;
carry the actual live-until from the data source.

### Upstream types that are NOT building blocks

- **`AutoRestoringSnapshotSource` is orthogonal.** It layers TTL bookkeeping and
  auto-restore over an inner source, is generic over `T`, and has no Unknown
  state — it always resolves Present/Absent. It does not give us fail-closed
  needs-state and is not part of the M1 mechanism.
- **No `LedgerEntryArchivalState` type exists** in the pinned tree; there is no
  ready-made archival/unknown enum to reuse. The tri-state distinction is ours
  to model.
- **`NetworkConfig::load_from_snapshot(&impl SnapshotSource)`** is the
  config-from-ledger mechanism: it reads the `ConfigSetting` entries through the
  same `SnapshotSource`, so under fail-closed Unknown each missing config entry
  emits a needs-state (see the config-prefetch mitigation in the design spec).

## Likely upstream contribution

Proposed smallest useful upstream patch:

> Make transaction snapshot storage and resolution backends injectable, and
> make absent-vs-unknown explicit.

Shape:

- split ledger-entry resolution from storage/cache backend;
- keep current native constructor for tests;
- add in-memory backend for portable use and tests;
- hide Cargo metadata, system dirs, filesystem locks, blocking HTTP, and compression behind native features;
- preserve actual TTL where data sources provide it (required — see
  `storage.rs:819-826`, above);
- make absent-vs-unknown explicit in the `SnapshotSource` contract.

Evidence backing the patch (all from the 28.0.2 pin):

- the `SnapshotSource` `Result<Option<(Rc<LedgerEntry>, Option<u32>)>, HostError>`
  contract has no first-class Unknown, forcing the sentinel workaround above;
- `simulation.rs:146-168` / `e2e_invoke.rs:870-872` show the host error is
  captured into `invoke_result`, so callers cannot cleanly distinguish
  "unresolved state" from "contract failure" without an out-of-band buffer;
- `AutoRestoringSnapshotSource` demonstrates upstream already layers behaviour
  over `SnapshotSource`, so an explicit absent-vs-unknown variant is a natural
  extension rather than a redesign.

## M3 RPC-equivalence findings

The M3 parity harness (`scripts/capture-parity-fixture.mjs` +
`packages/soroban-sim/src/differential/`) surfaced two upstream-relevant points,
both handled locally without a fork:

- **Config-setting prefetch is mandatory for a single-round RPC replay.** Because
  `NetworkConfig::load_from_snapshot` reads every `ConfigSetting` entry through
  the same fail-closed `SnapshotSource`, driving a captured RPC fixture through
  the `FromLedgerEntries` path needs-states each missing config entry. The
  capture script therefore snapshots *all* `ConfigSetting` keys (not just the
  invocation footprint) so the offline replay completes in one round with zero
  `needs-state` trips. This confirms the config-prefetch mitigation in the design
  spec and the `load_from_snapshot` note above.

- **Adjustment-config alignment is the sole source of resource/fee divergence.**
  `soroban-sim-core` runs with `SimulationAdjustmentConfig::no_adjustments()` and
  reports the host's raw metered instructions/fee; Stellar RPC applies its
  production `SimulationAdjustmentConfig` (instruction ~2.16x and fee safety
  multipliers). For the captured SAC `decimals()` case this is the *only*
  difference — result, footprint, auth, and events are byte-identical. If future
  work wants RPC-matching resource/fee numbers rather than raw host cost, the
  clean lever is to expose the adjustment config to callers instead of hard-coding
  `no_adjustments()`; the raw numbers remain the correct default for a portable
  simulator that must not bake in one network's fee policy.

- **`check_ledger_protocol_supported` hard-pins the host to its own protocol.**
  The recording-mode setup guard (`Host::set_ledger_info` →
  `check_ledger_protocol_supported`, `host.rs:572`) rejects any ledger protocol
  != the host's compiled protocol (28 for `soroban-env-host` 28.0.2) with a bare
  `Error(Context, InternalError)` thrown *before* diagnostics are enabled. This
  means a captured fixture cannot be replayed at its original protocol against a
  differently-versioned host; the replay must pin the ledger context to the
  host's protocol. Not a bug, but worth an upstream doc note that the failure
  mode for a protocol mismatch is an opaque internal error with no diagnostics.

## WASI/browser imports

Current component has no filesystem or socket imports, but does import WASI CLI/io/clock/random. This is likely from Rust `std`, Preview 2 ABI support, `getrandom`, and transitive crypto dependencies.

Potential upstream / local reductions:

- audit whether `getrandom` is retained only through unused crypto paths;
- test whether no-op custom getrandom backend removes `wasi:random/insecure-seed`;
- test `panic = "abort"` plus smaller std surface to reduce CLI imports;
- avoid `std::time`, environment, stdout/stderr in local code;
- keep ledger time and PRNG seed explicit regardless of imports.

## No fork yet

M0 required no fork or patch of `rs-soroban-env`, `rs-soroban-sdk`, Wasmi, Perch, Nido, or Stellar repositories.
