//! M1 three-state acceptance tests (issue #4 + the #3 rerun cases).
//!
//! Every scenario uses `UnmappedPolicy::Unknown` (fail-closed) so a not-yet-fetched
//! entry surfaces as `SimOutcome::NeedsState`. The needs-state buffer is authoritative:
//! a genuine contract trap comes back as `SimOutcome::Complete` with the error branch
//! (invoke_result tag byte = 1), never as NeedsState.
//!
//! Contract read order (empirically, and by host design): contract instance ->
//! contract code -> the storage entries the function touches. The source account is
//! NOT read for a plain contract invocation, so it never appears in needs-state.

use soroban_env_host::xdr::{ContractDataDurability, LedgerKey, Limits, ReadXdr, ScMap, ScVal, WriteXdr};
use soroban_sim_core::*;

/// `fixture_ledger_info().sequence_number`.
const SEQ: u32 = 1_000_000;
/// A live-until comfortably in the future (> SEQ, < max_entry_ttl).
const LIVE: u32 = 1_500_000;

// --- small builders ---------------------------------------------------------

fn p(entry: soroban_env_host::xdr::LedgerEntry, ttl: u32) -> LedgerEntryWithLiveUntil {
    LedgerEntryWithLiveUntil {
        entry,
        live_until_ledger: Some(ttl),
    }
}

fn code(wasm: &[u8]) -> LedgerEntryWithLiveUntil {
    p(contract_code_entry(wasm).unwrap(), LIVE)
}

fn inst(id: [u8; 32], wasm: &[u8], storage: Option<ScMap>) -> LedgerEntryWithLiveUntil {
    p(contract_instance_entry(id, wasm, storage).unwrap(), LIVE)
}

fn data(id: [u8; 32], key: u32, val: u32, dur: ContractDataDurability, ttl: u32) -> LedgerEntryWithLiveUntil {
    p(
        contract_data_entry(id, ScVal::U32(key), dur, ScVal::U32(val)),
        ttl,
    )
}

fn xdr(key: &LedgerKey) -> Vec<u8> {
    key.to_xdr(Limits::none()).unwrap()
}

fn run(
    hf: soroban_env_host::xdr::HostFunction,
    entries: Vec<LedgerEntryWithLiveUntil>,
    absent: Vec<LedgerKey>,
    policy: UnmappedPolicy,
) -> SimOutcome {
    simulate(integration_request(hf, entries, absent, policy)).unwrap()
}

/// Invoke-result discriminant byte: 0 = Ok, 1 = contract failure.
/// Layout: "SSIM0\0" (6) + instructions u32 (4) + memory u32 (4) = offset 14.
fn invoke_tag(bytes: &[u8]) -> u8 {
    bytes[14]
}

/// Decode the Ok `ScVal` from a Complete output (asserts the Ok branch).
fn ok_scval(bytes: &[u8]) -> ScVal {
    let mut o = 6 + 4 + 4;
    assert_eq!(bytes[o], 0, "expected Ok invoke_result branch");
    o += 1;
    let len = u32::from_be_bytes(bytes[o..o + 4].try_into().unwrap()) as usize;
    o += 4;
    ScVal::from_xdr(&bytes[o..o + len], Limits::none()).unwrap()
}

fn expect_needs(o: SimOutcome) -> Vec<LedgerKey> {
    match o {
        SimOutcome::NeedsState(keys) => keys,
        SimOutcome::Complete(b) => panic!("expected NeedsState, got Complete tag={}", invoke_tag(&b)),
    }
}

fn expect_complete(o: SimOutcome) -> Vec<u8> {
    match o {
        SimOutcome::Complete(b) => b,
        SimOutcome::NeedsState(keys) => panic!("expected Complete, got NeedsState {keys:?}"),
    }
}

// ===========================================================================
// (a) omit ContractCode -> NeedsState = exactly the ContractCode key
// ===========================================================================

#[test]
fn omitting_contract_code_needs_exactly_the_code_key() {
    let hf = storage_host_function("get_instance", vec![]).unwrap();
    // instance present (so code is the first Unknown read), code omitted.
    let entries = vec![inst(
        STORAGE_CONTRACT_ID,
        STORAGE_CONTRACT_WASM,
        Some(instance_storage_u32("INST", 42).unwrap()),
    )];

    let needed = expect_needs(run(hf, entries, vec![], UnmappedPolicy::Unknown));
    assert_eq!(needed.len(), 1, "one key per run (non-recoverable sentinel)");
    assert_eq!(
        xdr(&needed[0]),
        xdr(&contract_code_key(STORAGE_CONTRACT_WASM)),
        "needs-state must be the ContractCode LedgerKey (compared by XDR)"
    );
}

// ===========================================================================
// (b) omit the contract-instance ContractData -> NeedsState = that key
// ===========================================================================

#[test]
fn omitting_contract_instance_needs_exactly_the_instance_key() {
    let hf = storage_host_function("get_instance", vec![]).unwrap();
    let entries = vec![code(STORAGE_CONTRACT_WASM)];

    let needed = expect_needs(run(hf, entries, vec![], UnmappedPolicy::Unknown));
    assert_eq!(needed.len(), 1);
    assert_eq!(
        xdr(&needed[0]),
        xdr(&contract_instance_key(STORAGE_CONTRACT_ID)),
        "needs-state must be the contract-instance ContractData LedgerKey"
    );
}

// ===========================================================================
// (c) persistent + temporary data: missing / present / expired
// ===========================================================================

#[test]
fn persistent_data_missing_needs_the_data_key() {
    let hf = storage_host_function("get_persist", vec![ScVal::U32(7)]).unwrap();
    let entries = vec![
        code(STORAGE_CONTRACT_WASM),
        inst(STORAGE_CONTRACT_ID, STORAGE_CONTRACT_WASM, None),
    ];

    let needed = expect_needs(run(hf, entries, vec![], UnmappedPolicy::Unknown));
    assert_eq!(needed.len(), 1);
    assert_eq!(
        xdr(&needed[0]),
        xdr(&contract_data_key(
            STORAGE_CONTRACT_ID,
            ScVal::U32(7),
            ContractDataDurability::Persistent,
        )),
        "needs-state must be the persistent ContractData LedgerKey"
    );
}

#[test]
fn persistent_data_present_read_progresses_to_complete() {
    let hf = storage_host_function("get_persist", vec![ScVal::U32(7)]).unwrap();
    let entries = vec![
        code(STORAGE_CONTRACT_WASM),
        inst(STORAGE_CONTRACT_ID, STORAGE_CONTRACT_WASM, None),
        data(STORAGE_CONTRACT_ID, 7, 99, ContractDataDurability::Persistent, LIVE),
    ];

    let out = expect_complete(run(hf, entries, vec![], UnmappedPolicy::Unknown));
    assert_eq!(invoke_tag(&out), 0, "successful read");
    assert_eq!(ok_scval(&out), ScVal::U32(99));
}

#[test]
fn temporary_data_present_with_ttl_reads_ok() {
    let hf = storage_host_function("get_temp", vec![ScVal::U32(7)]).unwrap();
    let entries = vec![
        code(STORAGE_CONTRACT_WASM),
        inst(STORAGE_CONTRACT_ID, STORAGE_CONTRACT_WASM, None),
        // live_until > SEQ so the temporary entry is still alive.
        data(STORAGE_CONTRACT_ID, 7, 55, ContractDataDurability::Temporary, SEQ + 100),
    ];

    let out = expect_complete(run(hf, entries, vec![], UnmappedPolicy::Unknown));
    assert_eq!(invoke_tag(&out), 0);
    assert_eq!(ok_scval(&out), ScVal::U32(55));
}

#[test]
fn expired_temporary_data_takes_the_absent_path() {
    // Documented behavior: a temporary entry whose live_until < ledger sequence is
    // treated by the host as if it were absent (Ok(None) at the contract level).
    // The `storage` fixture's `get_temp` does `.unwrap()` on that None, so the
    // contract traps -> SimOutcome::Complete with the error branch (tag 1),
    // NOT NeedsState. The needs-state buffer stays empty.
    let hf = storage_host_function("get_temp", vec![ScVal::U32(7)]).unwrap();
    let entries = vec![
        code(STORAGE_CONTRACT_WASM),
        inst(STORAGE_CONTRACT_ID, STORAGE_CONTRACT_WASM, None),
        // live_until < SEQ -> expired.
        data(STORAGE_CONTRACT_ID, 7, 55, ContractDataDurability::Temporary, SEQ - 1),
    ];

    let out = expect_complete(run(hf, entries, vec![], UnmappedPolicy::Unknown));
    assert_eq!(
        invoke_tag(&out),
        1,
        "expired temporary -> absent -> unwrap trap -> contract failure, not needs-state"
    );
}

// ===========================================================================
// (d) nested: caller invoking add-i32 callee; omit callee code/instance
// ===========================================================================

#[test]
fn nested_call_with_callee_omitted_needs_callee_keys_then_completes() {
    let callee_instance = xdr(&contract_instance_key(CALLEE_CONTRACT_ID));
    let callee_code = xdr(&contract_code_key(FIXTURE_CONTRACT_WASM));

    // Round 1: caller code+instance present, callee omitted -> needs the callee
    // instance (read before the callee code).
    let hf = caller_host_function(CALLEE_CONTRACT_ID, 40, 2).unwrap();
    let entries = vec![
        code(CALLER_CONTRACT_WASM),
        inst(CALLER_CONTRACT_ID, CALLER_CONTRACT_WASM, None),
    ];
    let needed = expect_needs(run(hf, entries, vec![], UnmappedPolicy::Unknown));
    assert_eq!(needed.len(), 1);
    let first = xdr(&needed[0]);
    assert!(
        first == callee_instance || first == callee_code,
        "first missing callee key must be a callee code/instance key"
    );

    // Add both callee entries -> progresses to Complete with 40 + 2 = 42.
    let hf = caller_host_function(CALLEE_CONTRACT_ID, 40, 2).unwrap();
    let entries = vec![
        code(CALLER_CONTRACT_WASM),
        inst(CALLER_CONTRACT_ID, CALLER_CONTRACT_WASM, None),
        code(FIXTURE_CONTRACT_WASM),
        inst(CALLEE_CONTRACT_ID, FIXTURE_CONTRACT_WASM, None),
    ];
    let out = expect_complete(run(hf, entries, vec![], UnmappedPolicy::Unknown));
    assert_eq!(invoke_tag(&out), 0);
    assert_eq!(ok_scval(&out), ScVal::I32(42));

    // sanity: the two callee keys are the only cross-contract additions we needed.
    assert_ne!(callee_instance, callee_code);
}

// ===========================================================================
// (e) failure-after-resolution: a COMPLETE snapshot whose call genuinely fails
//     must be Complete(error branch), never NeedsState.
// ===========================================================================

#[test]
fn genuine_contract_failure_is_complete_not_needs_state() {
    // get_persist on a key that is proven ABSENT (known-absent): the read returns
    // Ok(None) and the fixture's `.unwrap()` traps. The needs-state buffer is empty,
    // so this classifies as Complete(err), not NeedsState.
    let missing = contract_data_key(STORAGE_CONTRACT_ID, ScVal::U32(7), ContractDataDurability::Persistent);
    let hf = storage_host_function("get_persist", vec![ScVal::U32(7)]).unwrap();
    let entries = vec![
        code(STORAGE_CONTRACT_WASM),
        inst(STORAGE_CONTRACT_ID, STORAGE_CONTRACT_WASM, None),
    ];

    let out = expect_complete(run(hf, entries, vec![missing], UnmappedPolicy::Unknown));
    assert_eq!(
        invoke_tag(&out),
        1,
        "trap on absent persistent key encodes as Complete error branch"
    );
}

// ===========================================================================
// (f) differential equivalence: iterative refetch converges to the same bytes
//     as a single complete-snapshot run.
// ===========================================================================

/// Drive the rerun protocol: start empty, resolve each NeedsState key against an
/// oracle (present if the oracle has it, otherwise known-absent) until Complete.
fn drive_to_complete(
    hf: soroban_env_host::xdr::HostFunction,
    oracle: &[LedgerEntryWithLiveUntil],
) -> Vec<u8> {
    let mut present: Vec<LedgerEntryWithLiveUntil> = Vec::new();
    let mut absent: Vec<LedgerKey> = Vec::new();

    for _ in 0..32 {
        let outcome = run(hf.clone(), present.clone(), absent.clone(), UnmappedPolicy::Unknown);
        match outcome {
            SimOutcome::Complete(bytes) => return bytes,
            SimOutcome::NeedsState(keys) => {
                assert!(!keys.is_empty(), "needs-state must be non-empty");
                for key in keys {
                    match oracle
                        .iter()
                        .find(|e| ledger_entry_to_ledger_key(&e.entry).unwrap() == key)
                    {
                        Some(entry) => present.push(entry.clone()),
                        None => absent.push(key),
                    }
                }
            }
        }
    }
    panic!("iterative refetch did not converge within the iteration budget");
}

fn single_complete_run(hf: soroban_env_host::xdr::HostFunction, oracle: Vec<LedgerEntryWithLiveUntil>) -> Vec<u8> {
    // Explicit complete snapshot: every present entry supplied, Absent policy for
    // anything else (there is nothing else to read).
    expect_complete(run(hf, oracle, vec![], UnmappedPolicy::Absent))
}

#[test]
fn iterative_refetch_matches_single_complete_run_storage() {
    let oracle = vec![
        code(STORAGE_CONTRACT_WASM),
        inst(STORAGE_CONTRACT_ID, STORAGE_CONTRACT_WASM, None),
        data(STORAGE_CONTRACT_ID, 7, 99, ContractDataDurability::Persistent, LIVE),
    ];
    let hf = storage_host_function("get_persist", vec![ScVal::U32(7)]).unwrap();

    let iterative = drive_to_complete(hf.clone(), &oracle);
    let complete = single_complete_run(hf, oracle);
    assert_eq!(
        iterative, complete,
        "iterative refetch must be byte-identical to the complete-snapshot run"
    );
    assert_eq!(ok_scval(&iterative), ScVal::U32(99));
}

#[test]
fn iterative_refetch_matches_single_complete_run_nested() {
    let oracle = vec![
        code(CALLER_CONTRACT_WASM),
        inst(CALLER_CONTRACT_ID, CALLER_CONTRACT_WASM, None),
        code(FIXTURE_CONTRACT_WASM),
        inst(CALLEE_CONTRACT_ID, FIXTURE_CONTRACT_WASM, None),
    ];
    let hf = caller_host_function(CALLEE_CONTRACT_ID, 40, 2).unwrap();

    let iterative = drive_to_complete(hf.clone(), &oracle);
    let complete = single_complete_run(hf, oracle);
    assert_eq!(iterative, complete);
    assert_eq!(ok_scval(&iterative), ScVal::I32(42));
}

/// Differential guard: injecting a known-absent key that the contract does not
/// read must not change a genuinely-present result, and must not loop.
#[test]
fn known_absent_key_does_not_change_present_result_or_loop() {
    let hf = storage_host_function("get_persist", vec![ScVal::U32(7)]).unwrap();
    let entries = vec![
        code(STORAGE_CONTRACT_WASM),
        inst(STORAGE_CONTRACT_ID, STORAGE_CONTRACT_WASM, None),
        data(STORAGE_CONTRACT_ID, 7, 99, ContractDataDurability::Persistent, LIVE),
    ];
    let baseline = expect_complete(run(hf.clone(), entries.clone(), vec![], UnmappedPolicy::Unknown));

    // Add an unrelated known-absent key; result bytes must be unchanged.
    let unrelated = contract_data_key(STORAGE_CONTRACT_ID, ScVal::U32(999), ContractDataDurability::Temporary);
    let with_absent = expect_complete(run(hf, entries, vec![unrelated], UnmappedPolicy::Unknown));
    assert_eq!(baseline, with_absent);
}
