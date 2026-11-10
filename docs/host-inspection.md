# Host selection and transaction inspection

`soroban-sim-core` selects exactly one host per build. Run a test binary for
each protocol to compare the same contracts and inputs across versions.

| Host feature | Recording host / simulation | Exact-apply feature | Isolated application host |
| --- | --- | --- | --- |
| `host-p27` | 27.0.1 | `exact-apply-p27` | `soroban-sim-host-p27` |
| `host-p29` (default) | 29.0.2 | `exact-apply` (default) | `soroban-sim-host` |
| `host-p30` | 30.0.0 | `exact-apply-p30` | `soroban-sim-host-p30` |

```sh
cargo test -p soroban-sim-core --no-default-features --features host-p27,exact-apply-p27
cargo test -p soroban-sim-core --no-default-features --features host-p29,exact-apply
cargo test -p soroban-sim-core --no-default-features --features host-p30,exact-apply-p30
```

A host feature alone selects only the recording host and simulation library;
it does not build an isolated application host. Pairing an exact-apply feature
with another line, or selecting more than one, is a compile error.

In a Rust consumer, disable defaults and select `host-p27` (or another line)
plus its exact-apply feature. Import XDR and host types from `soroban_sim_core::host`,
and simulation types from `soroban_sim_core::simulation`, so they belong to
the selected version. Selecting multiple lines is a compile error. Cargo
unifies features: test different versions in separate builds, not by enabling
all features. The CLI, RPC facade and browser component retain protocol 29.

`Session::new` still rejects forks whose protocol differs from the selected
host. Supply an appropriate snapshot and network configuration for each run;
do not relabel a real network snapshot to bypass parity. The common offline
test fixtures use contracts built with SDK 27 and a synthetic fork whose
protocol matches the selected host. Network replay fixtures remain tied to
the protocol at which they were captured.

## Authorization preparation

`prepare` preserves its existing defaults. `prepare_with(request, options)`
accepts `PrepareOptions`:

- `credential_kind: None` lets the registered signer select its existing format.
- `Some(CredentialKind::Address)`, `AddressV2`, or `AddressWithDelegates` explicitly
  selects a format. Incompatible signer choices fail instead of silently
  signing a different preimage.
- `allow_non_root_auth: true` records authorization rooted below the top call.

Existing classic and OZ adapters remain available. `AuthSigner::Custom` takes
a `CustomSigner::new(|payload_hash| ...)` callback returning the account's
signature `ScVal`. Its `DelegateSigner` tree can contain arbitrary contract
or classic-account signature adapters. The root and every nested delegate
receive the same address-bound root payload. Delegate lists are sorted and
duplicate siblings are rejected. Custom adapters must construct whatever
signature format their account verifies; the host still enforces it.

## Inspect the enforcing execution

```rust,ignore
let record = session.submit_with("case", &prepared.envelope, &SubmitOptions {
    trace: true,
    ..Default::default()
})?;
let trace = record.trace.as_ref().expect("host was invoked");
assert!(trace.complete);
```

Inspection requires the line's exact-apply feature; unsupported builds reject the request
before consuming a sequence or fee. It observes the original enforcing run,
including failed transactions, without requiring diagnostics or a replay.
Rejected transactions and failures before host execution have no host trace.
Standalone TTL maintenance does not enter a contract frame and has no host
trace; inspect its result, metrics and ledger diff.

`check_auth` lists every actual custom-account `__check_auth` frame, including
nested delegate calls. Each contains the account address, credential kind,
delegation root, exact payload hash, signature value, and ordered context
`ScVal`s. A credential kind is optional to represent a call whose signature
cannot be associated with a submitted credential. Contexts retain the exact
host representation, including creation contexts, rather than a lossy display.

`auth_failures` identifies `ArgsMismatch`, `NodeMissing`, `NonceReused`,
`SignatureExpired`, `SignatureExpirationTooLate`, and `CheckAuthRefused` with
its original `ScError`. Matching failures come from the host's eligible
invocation candidates; they are not inferred from diagnostic strings. A
refusal caught by a contract remains in the trace even if the transaction
succeeds. `NodeMissing` includes unavailable/exhausted authorization nodes.

`frames` assigns stable invocation-local IDs and parent IDs. `storage` records
ledger gets, existence checks, puts, deletes, TTL extensions and instance
subkey operations. `ledger_key` is the actual full ledger key when applicable;
individual instance subkeys instead carry their contract address and `key`.
Durability distinguishes temporary, persistent, instance and classic state.
Contract-code keys have no unique owning contract address. `succeeded` reports
the individual storage operation's result; `disposition` reports whether its
frame survived. Ancestor failure or a later transaction-level resource/rent
failure marks descendant writes rolled back, including a child that returned
successfully. A caught failed child does not roll back its successful parent.

Observation conversion uses the host's shadow budget and cannot fail the
transaction. Check `complete` before relying on trace absence or frame counts:
shadow-budget exhaustion makes observations incomplete. CPU, memory, fees,
return values and committed state are checked against untraced execution.

## Restore and extend TTL

`Session::prepare_maintenance(source, operation, keys, envelope_signers)`
simulates, budgets and signs an `OperationBody::RestoreFootprint` or
`OperationBody::ExtendFootprintTtl`. Submit the result using the normal
`submit` path. Hand-built envelopes use that same validation/application path.

Restore accepts only persistent read-write keys and an empty read-only
footprint. Archived entries must be available through `LedgerSource` or the
session overlay. It meters disk reads and writes, charges rent, and restores
to `current_ledger + min_persistent_entry_ttl - 1`. Missing and already-live
entries are skipped during application. A network provider that cannot fetch
archived values needs a snapshot containing them.

Extend accepts only Soroban read-only keys and an empty read-write footprint.
Live entries below the target become live through `current_ledger + extend_to`;
missing/archived entries and longer-lived entries are skipped. Since all
supported lines are protocol 23+, extension does not charge disk-read bytes.
Both operations require the operation source's low threshold, preserve normal
fee/sequence handling, stage changes until resources/rent are satisfied, and
produce their own XDR operation result variants. Failed operations refund
unused refundable fees and commit no storage changes.

## Add or upgrade a line

1. Pin matching released `soroban-env-host` and `soroban-simulation` versions.
   Verify the crates.io archive checksum. Copy upstream `src/`, `build.rs`,
   README and license into a separately named isolated host package, retaining
   its production dependencies and disabling upstream test/bench targets.
2. Record the original commit, archive checksum and file hashes in
   `UPSTREAM.json`. Apply only the observation additions in `host/inspection.rs`,
   host/frame trace accessors, auth refusal sites and storage entrypoints.
   Keep metered host algorithms unchanged. `PATCHES.json` records each changed
   file's original and patched hash, with a null original hash for a new file.
3. Add optional core dependencies, a mutually exclusive `host-pNN` feature,
   aliases in `lib.rs`, the workspace member and release-plz entry. Keep the
   exact host free of `recording_mode`/`testutils`. Extend `host_adapter.rs`
   only for actual enforcing-ABI changes (p27 uses parallel encoded TTL entries;
   p29/p30 use paired metadata).
4. Add the line to `host-lines` CI and this table. Run the complete common core
   suite, both with and without exact apply builds, provenance checks and the
   package rehearsal. If it becomes the CLI/browser default, upgrade their XDR
   dependencies together and recapture network parity fixtures deliberately.

```sh
python3 scripts/check-upstream-host.py
python3 scripts/sync-package-assets.py --check
python3 scripts/check-packages.py --allow-dirty  # local, before commit only
```

Do not mass-format vendored files. Review source differences against the
recorded upstream archive before updating `PATCHES.json`; a hash manifest is
provenance evidence, not proof that host behavior stayed unchanged.
