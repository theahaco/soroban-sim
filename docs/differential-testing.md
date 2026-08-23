# Differential testing plan

Status: **oracles A, B, and C are GREEN; RPC equivalence (M3) demonstrated for
the captured read-only case.** This document describes the oracles, comparison
fields, fuzz dimensions, and where each check lives.

- **Oracle A** (intra-native, iterative-rerun == complete run) —
  `crates/soroban-sim-core/tests/three_state.rs`. Green under
  `cargo test -p soroban-sim-core` (29 tests).
- **Oracle B** (native == wasmtime == browser, sha256 vs golden) —
  `fixtures/golden/`, the core-crate tests, `scripts/wasmtime-fixture.mjs`
  (`node scripts/wasmtime-fixture.mjs`), and the web-demo Playwright spec. Green
  across all three runtimes against the committed `add_i32` golden (388 bytes,
  sha256 prefix `1082a3e4`).
- **Oracle C** (order/batching convergence + absent-injection safety) —
  property harness in the core crate. Green under `cargo test -p soroban-sim-core`.
- **RPC equivalence (M3)** — `packages/soroban-sim/src/differential/`
  (`differential.test.ts`, `compare.ts`, `decode-ssim.ts`), driven under vitest.
  See "RPC equivalence (M3)" at the end for the actual parity result.

## Purpose

`soroban-sim` runs the same Soroban simulation logic in three places:

1. **native Rust** — `soroban-sim-core` linked against `soroban-simulation`
   `28.0.2`, driven by `cargo test`;
2. **wasmtime** — the `soroban-sim-component` WASM component invoked out of
   process (`scripts/wasmtime-fixture.mjs` / `pnpm run test:wasmtime`);
3. **browser Worker** — the same component loaded via jco in the web demo,
   exercised under Playwright.

Differential testing exists to prove that these three agree with each other and
with themselves across reruns, so that the iterative "needs-state" rerun
protocol can never silently diverge from a single complete-snapshot run.

## Invariants under test (shared by all oracles)

These are the M1 correctness contracts. Every oracle below is ultimately a way
of stress-testing them:

- **Three snapshot states.** A `get(key)` on the three-state snapshot resolves
  to exactly one of:
  - **Present** → `Ok(Some((Rc<LedgerEntry>, Option<u32>)))`;
  - **Absent** → `Ok(None)`;
  - **Unknown** → the key is recorded into a `RefCell` buffer **and** the call
    returns the sentinel `HostError::from((ScErrorType::Storage,
    ScErrorCode::ExceededLimit))`.
- **The recorded-keys buffer is the authoritative needs-state signal.** A `get()`
  returning the sentinel does **not** surface as the outer `Result::Err` of
  `simulate_invoke_host_function_op`; it comes back as
  `Ok(InvokeHostFunctionSimulationResult { invoke_result: Err(sentinel), .. })`.
  Classification is therefore driven by `probe.take_needed()`:
  - non-empty buffer ⇒ **NeedsState** (rerun with more entries);
  - empty buffer ⇒ **Complete** (encode the result; a genuine contract failure
    is encoded inline, it is *not* a needs-state).
- **Sentinel matching is structural, never string-based.** Tests assert on
  `e.error.is_type(ScErrorType::Storage) && e.error.is_code(ScErrorCode::ExceededLimit)`,
  never on formatted error text.
- **Present contract entries carry TTL.** Present `ContractData` / `ContractCode`
  entries MUST carry `Some(live_until_ledger)`; a missing TTL makes the host
  abort with `(Storage, InternalError)`. A malformed-fixture test guards this.
- **Byte-stable encoding.** `encode_simulation_result` output is byte-identical
  across runtimes and reruns. The `add_i32` golden is the anchor: 388 bytes,
  sha256 prefix `1082a3e4`. `run_fixture()` and
  `fixtures/golden/add_i32.fixture.json` must not change behavior.

A dedicated test asserts that a genuine budget/limit failure
(`Budget`/`Context` `ExceededLimit`) is classified **Complete**, not NeedsState —
i.e. only *our* `get()` ever writes the recorded-keys buffer, so the buffer, not
the error code alone, is the discriminator.

## Oracle A — intra-native equivalence (iterative rerun == single complete run)

**Claim.** For a fixed complete snapshot, an *iterative-rerun* simulation — start
from a partial snapshot, and each round resolve the Unknown keys reported by the
previous round from a complete oracle, feeding them back as seed entries —
produces **byte-identical** SSIM output to a *single complete-snapshot* run of
the same invocation.

**Mechanism.**
- Build a complete `ThreeStateSnapshot` (the oracle) for a fixture invocation.
- Run once with the complete snapshot → `Complete(bytes_ref)`.
- Run the same invocation starting from an empty/partial snapshot; on each
  `NeedsState(keys)`, resolve `keys` from the oracle, add them as seed entries,
  and rerun until `Complete(bytes_iter)`.
- Assert `bytes_iter == bytes_ref` (and therefore equal sha256).

**Where it lives.** `crates/soroban-sim-core/tests/three_state.rs`. Uses the
`storage` and `caller` fixture contracts (single storage read/write; single
cross-contract call) so that at least one multi-round resolution occurs.

## Oracle B — cross-runtime agreement (native == wasmtime == browser)

**Claim.** For the same input, native Rust, the wasmtime `--invoke` of the
component, and the browser Worker produce output with an **identical sha256**,
anchored to the committed goldens.

**Mechanism.**
- `fixtures/golden/*.fixture.json` holds input plus expected output length and
  sha256. `add_i32` exists today; add `storage` and `nested`/`caller` goldens
  once their bytes are stable.
- Native: `cargo test -p soroban-sim-core` recomputes and compares.
- wasmtime: `scripts/wasmtime-fixture.mjs` (`pnpm run test:wasmtime`) invokes the
  component and compares sha256 to the golden.
- Browser: a Playwright test in the web demo loads the component via jco in a
  Worker, runs the same fixture, and compares sha256 to the golden.

Because all three compare to the *same* golden file, agreement is transitive:
any runtime that drifts fails against the shared anchor.

**Where it lives.** goldens in `fixtures/golden/`; native harness in the core
crate tests; `scripts/wasmtime-fixture.mjs`; web-demo Playwright spec.

## Oracle C — property tests (order/batching convergence + absent-injection safety)

**Claim 1 (convergence).** For a fixed complete snapshot, resolving Unknown keys
in **any order** and with **any batching** converges to the same Complete bytes.
Whether the client resolves one key per round, all keys at once, or any
partition in between, the final encoded output is identical.

**Claim 2 (absent-injection safety).** Injecting **known-absent** keys (entries
the client asserts do not exist) never changes a genuinely-**present** result.
Marking irrelevant keys absent must not perturb a simulation whose footprint
does not touch them.

**Mechanism.** Property/fuzz harness (proptest-style) that, from a fixed
complete oracle:
- shuffles the omission order of the initially-Unknown keys and randomly
  partitions them into rerun batches, asserting the terminal bytes are invariant;
- injects random sets of known-absent keys disjoint from the true footprint and
  asserts the Present result and its bytes are unchanged;
- exercises TTL boundaries (see fuzz dimensions).

**Where it lives.** core crate unit/integration tests (property harness in
`crates/soroban-sim-core`), reusing the `three_state.rs` fixtures.

## Comparison fields

When two runs are compared (A, B, or C), equality is asserted on the encoded
SSIM output as a whole (byte-identical / sha256) and, where a run is decoded for
diagnosis, field-by-field on:

- **invocation result** — the returned `ScVal` (or the inline contract failure);
- **auth entries** — recorded `SorobanAuthorizationEntry` set;
- **contract events** — emitted events, in order;
- **diagnostics** — compared only where stable across runtimes; volatile or
  non-deterministic diagnostic text is excluded from the equality set;
- **footprint** — read-only and read-write `LedgerKey` sets;
- **resource estimates** — instructions, read/write bytes, entry counts;
- **fees** — resource fee and any fee breakdown;
- **modified entries** — the post-invocation ledger entry changes (`LedgerEntry`
  values and TTL bumps);
- **failures** — for the genuine-failure cases, the structured error
  (type/code), matched via `is_type`/`is_code`, never by string.

The byte-identical output is the primary oracle; the field-level list is the
decoded view used to localize a mismatch when the bytes differ.

## Fuzz dimensions

- **Key omission order** — the order in which initially-Unknown keys are
  revealed to the simulation across rerun rounds (Oracle C, Claim 1).
- **Batching** — the partition of Unknown keys into rerun batches (one-per-round
  through all-at-once) (Oracle C, Claim 1).
- **Absent vs present** — for each candidate key, whether it is genuinely
  present, genuinely absent, or injected as known-absent; and whether injected
  absent keys are disjoint from the footprint (Oracle C, Claim 2).
- **TTL boundaries** — Present entries with `live_until_ledger` at, just below,
  and just above the current ledger sequence (live, expiring-this-ledger,
  archived), to exercise the `Some(live_until)` requirement and TTL-driven
  behavior.

## Summary: where each check lives

| Oracle | Check | Location |
| --- | --- | --- |
| A | iterative rerun == complete run (byte-identical) | `crates/soroban-sim-core/tests/three_state.rs` |
| B | native == wasmtime == browser (sha256 vs golden) | `fixtures/golden/`, core tests, `scripts/wasmtime-fixture.mjs`, web-demo Playwright |
| C | order/batching convergence; absent-injection safety | core property/integration tests (reusing `three_state.rs` fixtures) |

## RPC equivalence (M3)

Oracles A, B, and C above are green, so the M3 RPC-equivalence oracle is now
active. It compares `soroban-sim`'s output against a real Stellar testnet RPC
`simulateTransaction` response, field-by-field.

**Harness.** `scripts/capture-parity-fixture.mjs` (`pnpm run capture:parity`)
captures a live RPC `simulateTransaction` plus the full ledger state needed to
replay it offline — the footprint entries and every `ConfigSetting` entry that
`NetworkConfig::load_from_snapshot` reads — into a committed JSON fixture under
`fixtures/parity/`. The differential test
(`packages/soroban-sim/src/differential/differential.test.ts`, decode/compare in
`decode-ssim.ts` + `compare.ts`) drives `soroban-sim` over that fixture through
the `FromLedgerEntries` config path (no `needs-state` round trip) and compares
the two outputs. It runs under `pnpm --filter @theahaco/soroban-sim test`.

**Fixture.** `fixtures/parity/native-sac-decimals.json` — a read-only invoke of
the native Stellar Asset Contract's `decimals()` (contract
`CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC` on testnet),
returning `7`. The native SAC is a built-in contract (executable
`StellarAsset`), so there is no `ContractCode` wasm to fetch — the footprint is
just the SAC instance `ContractData` entry.

**Parity result (captured case).** All MUST-match fields are byte-identical
between `soroban-sim` and RPC:

| Field | Verdict | Detail |
| --- | --- | --- |
| invocation result (`ScVal`) | **match** | local `7` == RPC `7`, byte-identical (`AAAAAwAAAAc=`) |
| footprint read-only | **match** | 1 key each, byte-identical (SAC instance `ContractData`) |
| footprint read-write | **match** | 0 == 0 |
| auth entries | **match** | 0 == 0 |
| contract events | **match** | 0 == 0 (RPC's 2 events are diagnostic `fn_call`/`fn_return`, not contract events) |
| read bytes / write bytes | **match** | 0 == 0 |
| diagnostic events (count) | **match** | 2 == 2 |
| instructions | **divergent-expected** | local `59125` vs RPC `127569` (~2.16x) |
| resource fee (stroops) | **divergent-expected** | local `10045` vs RPC `minResourceFee` `12279` |

The instruction and fee divergences are **expected, not bugs**: `soroban-sim-core`
runs with `SimulationAdjustmentConfig::no_adjustments()`, reporting the host's
raw metered cost, whereas Stellar RPC applies its production
`SimulationAdjustmentConfig` (instruction and fee safety multipliers). The test
asserts the result / footprint-RO / footprint-RW verdicts are `match` and that
the MISMATCH set is empty; resource/fee fields are allowed to be
`divergent-expected`.

**Protocol-version note (a real finding, root-caused — not papered over).** The
fixture is captured against testnet at ledger protocol 27, but `soroban-sim-core`
links `soroban-env-host` 28.0.2, whose network interface protocol is 28. The
host's replay-safety guard `check_ledger_protocol_supported` (invoked from
`Host::set_ledger_info` during recording *setup*, `host.rs:572`) rejects any
ledger protocol != 28 with a bare `Error(Context, InternalError)` — thrown
before diagnostics are enabled, which is why the failure carries no debug info
and no `fn_call` event. Driving the fixture as-captured (pv 27) therefore yields
`Complete`-with-inline-error and ~3801 instructions. `decimals()` is
protocol-invariant, so the harness pins the local ledger context to the host's
supported protocol (`HOST_PROTOCOL_VERSION = 28`); result and footprint parity
are unaffected. Verified empirically: pv 28 ⇒ result `7`; pv 22–27 ⇒ "too old";
pv 29 ⇒ "too new".

**Scope — do not overclaim.** This demonstrates RPC parity for the captured
read-only SAC case: the invocation result and footprint match a live RPC
byte-for-byte, and resource/fee differences are fully explained by adjustment
config. It does **not** yet establish general RPC authority across arbitrary
contracts, write footprints, auth trees, or state-archival/restore paths. Those
extend the fixture set via `pnpm run capture:parity` and are future work.

**Re-capturing.** Run `pnpm run capture:parity` (needs live testnet RPC
reachability) to refresh `fixtures/parity/native-sac-decimals.json`; the
differential test then replays it entirely offline.
