# Architecture

## Boundary

Soroban Sim wraps the Soroban simulator in a WASI Preview 2 component. It does **not** put a component boundary between the Soroban host and individual contracts.

```text
Browser UI
  -> TypeScript API + Web Worker
  -> Jco-transpiled WASI Preview 2 component
  -> soroban-simulation
  -> soroban-env-host
  -> soroban-wasmi
  -> existing wasm32v1-none contract bytes
```

Individual contracts stay normal Soroban core Wasm. The host loads contract code through the same Wasmi path used by native Soroban host execution.

## Crates and packages

- `crates/soroban-sim-core`: Rust simulation core. Owns in-memory snapshots, fixture construction, simulation call, and deterministic output encoding.
- `crates/soroban-sim-component`: WIT export wrapper for `wasm32-wasip2`.
- `packages/soroban-sim`: TypeScript package placeholder around Jco output.
- `packages/web-demo`: Vite + Playwright browser Worker proof.

## Current WIT

`wit/simulator.wit` exports:

- `run-fixture() -> result<list<u8>, string>` for Milestone 0 proof.
- `simulate(request) -> result<outcome, string>` for host-function simulation.

The request carries XDR bytes for host function, source account, and ledger entries. Ledger context is a small WIT record because `LedgerInfo` is host-side context, not Stellar XDR.

`request` fields:

- `host-function-xdr` / `source-account-xdr`: stellar-xdr `HostFunction` and `AccountId` (transaction-envelope XDR is deferred past M1).
- `ledger-context`: host-side `LedgerInfo`.
- `ledger-entries`: entries the caller **knows are present**, each with an optional `live-until-ledger`. `ContractData` / `ContractCode` entries MUST carry a TTL or the host aborts. Any `ConfigSetting` entries live here too (see network config below).
- `known-absent-keys: list<list<u8>>`: `LedgerKey` XDR the caller has **proven absent**. These map to `Ok(None)` in the snapshot and are never re-emitted as needs-state, which is what stops the rerun loop from re-requesting a confirmed-absent key.
- `base-prng-seed`, `enable-diagnostics`: determinism + diagnostic-event toggle.

`outcome` is a variant:

- `complete(list<u8>)`: the deterministic SSIM-encoded result. A genuine contract failure is encoded **inline** here (not surfaced as an error).
- `needs-state(list<list<u8>>)`: `LedgerKey` XDR that were read while Unknown. The caller refetches them and reruns.

`result::err(string)` is reserved for real decode / infrastructure errors, never for missing state.

Network config is loaded from `ConfigSetting` ledger entries supplied through `ledger-entries` (via `NetworkConfig::load_from_snapshot`), so it is **not** a WIT field. There is no canonical `NetworkConfig` XDR to encode; config settings are ordinary fetchable state, so a missing one produces needs-state like any other key. The component sets `NetworkConfigSource::FromLedgerEntries`; the fixture / native complete-snapshot path uses an explicit fixture config to keep the golden stable.

## Fixture execution path

The M0 fixture:

1. builds `fixtures/contracts/add-i32` as `wasm32v1-none`;
2. stores the compiled contract bytes in `fixtures/contracts/add_i32.wasm`;
3. creates `ContractCode` and `ContractData` ledger entries in memory;
4. invokes `add(40, 2)` through `HostFunction::InvokeContract`;
5. runs `soroban-simulation::simulate_invoke_host_function_op`;
6. encodes invocation result, auth, events, diagnostics, transaction data, measurements, and modified entries into deterministic bytes;
7. compares native / Wasmtime / browser Worker SHA-256.

## State-loading model for Milestone 1

Snapshot lookups distinguish three states, supplied by the caller through the request:

- **present**: ledger entry plus actual TTL (`ledger-entries`);
- **absent**: proven absence (`known-absent-keys`), returned to the host as `Ok(None)`;
- **unknown**: neither supplied — a cache miss / not yet resolved.

`SnapshotSource::get` returns `Option<EntryWithLiveUntil>`, so `None` already means absent to the host. Unknown cannot be represented as `None`, so it is handled out of band.

`ThreeStateSnapshot` (in `crates/soroban-sim-core/src/snapshot.rs`) implements this:

1. A key in `ledger-entries` resolves to `Ok(Some(entry))`; a key in `known-absent-keys` resolves to `Ok(None)`.
2. Any other key follows `UnmappedPolicy`. The WIT `simulate` path uses `UnmappedPolicy::Unknown` (fail-closed): the key XDR is recorded into a `RefCell<BTreeSet<LedgerKey>>` buffer and `get` returns the **sentinel** error `(ScErrorType::Storage, ScErrorCode::ExceededLimit)`. The fixture / complete-snapshot path uses `UnmappedPolicy::Absent`, matching the old in-memory source and preserving the golden.
3. The sentinel is chosen because it is non-recoverable (a nested `try_call` cannot swallow it), is never produced by the env-host in recording mode, and survives error decoration unchanged.

**Needs-state classification is buffer-authoritative.** A `get` error does not surface as the outer `Result::Err` of `simulate_invoke_host_function_op`; the host's post-invocation footprint re-scan returns `Ok(result)` with `invoke_result = Err(sentinel)`. So after the call the core checks `probe.take_needed()` **first**: if the recorded-keys buffer is non-empty the outcome is `NeedsState(keys)`; otherwise it is `Complete` (a genuine contract failure is encoded inline in the Complete bytes). The buffer — written only by our `get` — is the sole needs-state trigger; `is_unknown_state_error` is only a debug aid.

Local protocol:

1. The snapshot source classifies each key `Present` / `Absent` / `Unknown` from the request.
2. On an Unknown lookup it records the ledger-key XDR and aborts with the sentinel.
3. The wrapper maps a non-empty recorded-key set to `outcome::needs-state(keys)` (LedgerKey XDR).
4. JavaScript fetches those keys asynchronously via RPC or another provider.
5. The response, including known absence, is persisted in IndexedDB under the exact context and fed back as `ledger-entries` / `known-absent-keys` on rerun.
6. Simulation reruns deterministically from the beginning; feeding refetched-absent keys as `known-absent-keys` guarantees they are not re-requested, so the loop terminates.

First implementation discovers one key per run (the sentinel aborts on the first Unknown read). Batching comes after correctness.

## Cache identity

Browser cache keys must include:

- network passphrase or network ID;
- ledger sequence;
- optional transaction hash / transaction position;
- ledger-key XDR hash;
- protocol version where needed.

The cache is **tri-state**: present entries (with their real TTL) and explicit absent markers are both stored. Caching absence is required — a confirmed-absent key must be feedable as `known-absent-keys` on rerun so it is never re-requested, otherwise the needs-state loop cannot terminate.

TTL must be preserved from source data. No synthetic `u32::MAX` TTL for transaction simulation.

## Browser model

The component runs in a dedicated Worker. Main-thread API should:

- create/terminate worker;
- pass explicit ledger context and deterministic seed;
- enforce iteration and input limits;
- support cancellation via Worker termination;
- never hold secret keys or request signatures.

## Non-goals kept out of the architecture

- Converting Soroban contracts into components.
- Describing all host functions in WIT.
- Static composition of contracts with simulator.
- Replacing Wasmi with browser-native `WebAssembly.instantiate` for contracts.
- Running ordinary WASI components as Soroban contracts.
