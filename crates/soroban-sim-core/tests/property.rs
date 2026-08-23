//! Oracle C — deterministic property tests (M3).
//!
//! These are the "property/fuzz" oracle from `docs/differential-testing.md`, but
//! driven by **exhaustive deterministic enumeration** rather than randomness (no
//! `rand`, no clock): the initially-Unknown footprint set is small and fixed, so
//! every permutation and a representative set of batchings can be run in full.
//!
//! Fixture: the `caller` contract making a nested `invoke_contract` into the
//! `add-i32` callee. Its footprint has four initially-Unknown keys —
//!   caller instance, caller code, callee instance, callee code —
//! plus the source account, which is NOT read for a contract invocation and so
//! never enters needs-state (asserted by discovery below).
//!
//! Claim 1 (convergence): resolving those keys in ANY order and with ANY batching
//! converges to byte-identical (and thus sha256-identical) SSIM output as a single
//! complete-snapshot run.
//!
//! Claim 2 (absent-injection safety): injecting known-absent keys DISJOINT from the
//! footprint never perturbs the present result; marking a key that IS in the
//! footprint absent instead yields a DIFFERENT Complete result (a (Storage,
//! MissingValue) contract failure encoded inline), never NeedsState.
//!
//! Plus TTL boundaries (fuzz dimension): live_until at / just-above / just-below the
//! ledger sequence, and the `Some(live_until)` requirement.

use soroban_env_host::xdr::{HostFunction, LedgerEntry, LedgerKey, Limits, ScVal, WriteXdr};
use soroban_sim_core::*;

/// `fixture_ledger_info().sequence_number`.
const SEQ: u32 = 1_000_000;
/// A live-until comfortably in the future (> SEQ, < max_entry_ttl).
const LIVE: u32 = 1_500_000;

// --- small builders (mirrored from three_state.rs) -------------------------

fn p(entry: LedgerEntry, ttl: Option<u32>) -> LedgerEntryWithLiveUntil {
    LedgerEntryWithLiveUntil { entry, live_until_ledger: ttl }
}
fn code(wasm: &[u8], ttl: u32) -> LedgerEntryWithLiveUntil {
    p(contract_code_entry(wasm).unwrap(), Some(ttl))
}
fn inst(id: [u8; 32], wasm: &[u8], ttl: u32) -> LedgerEntryWithLiveUntil {
    p(contract_instance_entry(id, wasm, None).unwrap(), Some(ttl))
}

fn run(
    hf: HostFunction,
    entries: Vec<LedgerEntryWithLiveUntil>,
    absent: Vec<LedgerKey>,
    policy: UnmappedPolicy,
) -> SimOutcome {
    simulate(integration_request(hf, entries, absent, policy)).unwrap()
}

/// Invoke-result discriminant byte: 0 = Ok, 1 = contract/host failure.
/// Layout: "SSIM0\0" (6) + instructions u32 (4) + memory u32 (4) = offset 14.
fn invoke_tag(bytes: &[u8]) -> u8 {
    bytes[14]
}

/// Decode the error text from a Complete output (asserts the Err branch).
fn err_text(bytes: &[u8]) -> String {
    let mut o = 6 + 4 + 4;
    assert_eq!(bytes[o], 1, "expected Err invoke_result branch");
    o += 1;
    let len = u32::from_be_bytes(bytes[o..o + 4].try_into().unwrap()) as usize;
    o += 4;
    String::from_utf8_lossy(&bytes[o..o + len]).into_owned()
}

fn expect_complete(o: SimOutcome) -> Vec<u8> {
    match o {
        SimOutcome::Complete(b) => b,
        SimOutcome::NeedsState(keys) => panic!("expected Complete, got NeedsState {keys:?}"),
    }
}

fn key_of(e: &LedgerEntryWithLiveUntil) -> LedgerKey {
    ledger_entry_to_ledger_key(&e.entry).unwrap()
}

fn xdr(k: &LedgerKey) -> Vec<u8> {
    k.to_xdr(Limits::none()).unwrap()
}

/// The complete oracle snapshot for the nested caller -> add-i32 invocation:
/// caller code + instance, callee code + instance, all live.
fn oracle() -> Vec<LedgerEntryWithLiveUntil> {
    vec![
        code(CALLER_CONTRACT_WASM, LIVE),
        inst(CALLER_CONTRACT_ID, CALLER_CONTRACT_WASM, LIVE),
        code(FIXTURE_CONTRACT_WASM, LIVE),
        inst(CALLEE_CONTRACT_ID, FIXTURE_CONTRACT_WASM, LIVE),
    ]
}

fn nested_hf() -> HostFunction {
    caller_host_function(CALLEE_CONTRACT_ID, FIXTURE_A, FIXTURE_B).unwrap()
}

/// Reference bytes: a single complete-snapshot run, Explicit config, Absent policy.
fn reference_bytes() -> Vec<u8> {
    expect_complete(run(nested_hf(), oracle(), vec![], UnmappedPolicy::Absent))
}

/// Find the present oracle entry for a key (the key is always in the footprint here).
fn resolve(key: &LedgerKey) -> LedgerEntryWithLiveUntil {
    oracle()
        .into_iter()
        .find(|e| key_of(e) == *key)
        .unwrap_or_else(|| panic!("key not in oracle: {key:?}"))
}

/// Discover the initially-Unknown footprint keys by driving from an empty snapshot
/// with `UnmappedPolicy::Unknown` and accumulating the NeedsState keys across rounds
/// (resolving each from the oracle so the next round can progress).
fn discover_footprint() -> Vec<LedgerKey> {
    let hf = nested_hf();
    let mut present: Vec<LedgerEntryWithLiveUntil> = Vec::new();
    let mut discovered: Vec<LedgerKey> = Vec::new();
    for _ in 0..32 {
        match run(hf.clone(), present.clone(), vec![], UnmappedPolicy::Unknown) {
            SimOutcome::Complete(_) => return discovered,
            SimOutcome::NeedsState(keys) => {
                assert!(!keys.is_empty(), "needs-state must be non-empty");
                for k in keys {
                    // Source account must never appear in the footprint of a contract
                    // invocation.
                    assert_ne!(
                        xdr(&k),
                        xdr(&account_key(FIXTURE_SOURCE_ACCOUNT)),
                        "source account must not be read by a contract invocation"
                    );
                    if !discovered.iter().any(|d| xdr(d) == xdr(&k)) {
                        discovered.push(k.clone());
                    }
                    present.push(resolve(&k));
                }
            }
        }
    }
    panic!("footprint discovery did not converge");
}

/// All permutations of `items` (recursive; n! for the small fixed footprint).
fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for i in 0..items.len() {
        let mut rest = items.to_vec();
        let head = rest.remove(i);
        for mut tail in permutations(&rest) {
            let mut v = vec![head.clone()];
            v.append(&mut tail);
            out.push(v);
        }
    }
    out
}

/// Drive the iterative-rerun loop, supplying the footprint keys in `order`,
/// batched by the `groups` composition (each group = keys supplied that round).
/// Returns the terminal Complete bytes.
fn drive_ordered(order: &[LedgerKey], groups: &[usize]) -> Vec<u8> {
    let hf = nested_hf();
    let mut present: Vec<LedgerEntryWithLiveUntil> = Vec::new();
    let mut idx = 0usize;
    let mut group_iter = groups.iter();
    loop {
        match run(hf.clone(), present.clone(), vec![], UnmappedPolicy::Unknown) {
            SimOutcome::Complete(bytes) => return bytes,
            SimOutcome::NeedsState(_) => {
                let g = *group_iter
                    .next()
                    .expect("ran out of batches before reaching Complete");
                for _ in 0..g {
                    let key = &order[idx];
                    idx += 1;
                    present.push(resolve(key));
                }
            }
        }
    }
}

// ===========================================================================
// Setup sanity: discovery finds exactly the four footprint keys.
// ===========================================================================

#[test]
fn footprint_discovery_finds_the_four_contract_keys() {
    let discovered = discover_footprint();
    assert_eq!(
        discovered.len(),
        4,
        "nested caller->callee footprint = caller inst/code + callee inst/code"
    );

    // The discovered set equals exactly {caller inst, caller code, callee inst,
    // callee code} (compared by XDR, order-independent).
    let mut got: Vec<Vec<u8>> = discovered.iter().map(xdr).collect();
    got.sort();
    let mut want: Vec<Vec<u8>> = [
        contract_instance_key(CALLER_CONTRACT_ID),
        contract_code_key(CALLER_CONTRACT_WASM),
        contract_instance_key(CALLEE_CONTRACT_ID),
        contract_code_key(FIXTURE_CONTRACT_WASM),
    ]
    .iter()
    .map(xdr)
    .collect();
    want.sort();
    assert_eq!(got, want, "footprint = the four contract code/instance keys");
}

// ===========================================================================
// CLAIM 1 — order/batching convergence (exhaustive deterministic enumeration).
//
// For EVERY permutation of the footprint set and a representative set of
// batchings (one-per-round, all-at-once, and three intermediate partitions),
// the terminal Complete bytes are byte-identical (and sha256-identical) to the
// reference single-complete-snapshot run.
// ===========================================================================

#[test]
fn claim1_order_and_batching_converge_to_reference_bytes() {
    let footprint = discover_footprint();
    assert_eq!(footprint.len(), 4);

    let reference = reference_bytes();
    let reference_sha = wasm_hash(&reference); // sha256(SSIM output)
    assert_eq!(invoke_tag(&reference), 0, "reference is a successful invoke");

    // Compositions of 4: one-per-round, all-at-once, and three intermediate
    // partitions (>= two intermediate as required).
    let batchings: Vec<Vec<usize>> = vec![
        vec![1, 1, 1, 1], // one key per round
        vec![4],          // all at once
        vec![2, 2],       // intermediate
        vec![1, 2, 1],    // intermediate
        vec![3, 1],       // intermediate
    ];
    for b in &batchings {
        assert_eq!(b.iter().sum::<usize>(), footprint.len(), "composition covers all keys");
    }

    let perms = permutations(&footprint);
    assert_eq!(perms.len(), 24, "4! permutations");

    let mut runs = 0usize;
    for perm in &perms {
        for groups in &batchings {
            let terminal = drive_ordered(perm, groups);
            assert_eq!(
                terminal, reference,
                "order/batching must not change output (perm={:?} groups={:?})",
                perm.iter().map(xdr).map(|b| b.len()).collect::<Vec<_>>(),
                groups
            );
            assert_eq!(
                wasm_hash(&terminal),
                reference_sha,
                "byte-identity implies sha256-identity"
            );
            runs += 1;
        }
    }

    assert_eq!(runs, 24 * 5, "24 permutations x 5 batchings");
    // Deterministic-enumeration coverage log (visible with --nocapture).
    println!(
        "CLAIM 1: exercised {} permutations x {} batchings = {} convergent runs",
        perms.len(),
        batchings.len(),
        runs
    );
}

// ===========================================================================
// CLAIM 2 — absent-injection safety.
// ===========================================================================

/// Injecting known-absent keys that are DISJOINT from the footprint must not
/// change the present result's bytes.
#[test]
fn claim2_disjoint_absent_keys_do_not_perturb_present_result() {
    let reference = reference_bytes();

    // A deterministic set of known-absent keys, each provably off the footprint:
    // ContractData for a different contract id / different sc keys / durability,
    // a bogus ContractCode hash, and an unrelated account.
    let disjoint: Vec<LedgerKey> = vec![
        contract_data_key(
            STORAGE_CONTRACT_ID,
            ScVal::U32(1),
            soroban_env_host::xdr::ContractDataDurability::Persistent,
        ),
        contract_data_key(
            STORAGE_CONTRACT_ID,
            ScVal::U32(2),
            soroban_env_host::xdr::ContractDataDurability::Temporary,
        ),
        contract_data_key(
            [99; 32],
            ScVal::Symbol("nope".try_into().unwrap()),
            soroban_env_host::xdr::ContractDataDurability::Persistent,
        ),
        // Bogus ContractCode hash: sha256 of bytes that are not any fixture wasm.
        contract_code_key(b"not-a-real-contract-wasm-blob"),
        account_key([200; 32]),
    ];

    // Sanity: none of the injected absent keys collides with the true footprint.
    let footprint: Vec<Vec<u8>> = discover_footprint().iter().map(xdr).collect();
    for k in &disjoint {
        assert!(
            !footprint.contains(&xdr(k)),
            "injected absent key must be disjoint from the footprint: {k:?}"
        );
    }

    let with_absent =
        expect_complete(run(nested_hf(), oracle(), disjoint, UnmappedPolicy::Absent));
    assert_eq!(
        with_absent, reference,
        "known-absent keys off the footprint must not perturb a present result"
    );
    assert_eq!(wasm_hash(&with_absent), wasm_hash(&reference));
}

/// Marking a key that IS in the footprint absent yields a DIFFERENT Complete
/// result — specifically a (Storage, MissingValue) contract failure encoded
/// inline (tag 1) — and NEVER NeedsState (the buffer stays empty under Absent
/// policy). This proves the injection is only *safe* when disjoint.
#[test]
fn claim2_footprint_key_marked_absent_changes_outcome_and_is_not_needs_state() {
    let reference = reference_bytes();
    assert_eq!(invoke_tag(&reference), 0, "reference is a successful invoke");

    for key in discover_footprint() {
        // Full present snapshot, but this one footprint key overridden to Absent.
        // (ThreeStateSnapshot inserts absent keys after present, so Absent wins.)
        let outcome = run(nested_hf(), oracle(), vec![key.clone()], UnmappedPolicy::Absent);

        // Must be Complete (buffer empty under Absent policy), never NeedsState.
        let bytes = expect_complete(outcome);

        // A genuine contract failure encoded inline, not a success, and different
        // bytes than the reference.
        assert_eq!(
            invoke_tag(&bytes),
            1,
            "a missing footprint entry must fail the invoke (key={key:?})"
        );
        assert_ne!(
            bytes, reference,
            "marking a real footprint key absent must change the output (key={key:?})"
        );
        let text = err_text(&bytes);
        assert!(
            text.contains("Storage") && text.contains("MissingValue"),
            "expected a (Storage, MissingValue) failure, got: {text}"
        );
    }
}

// ===========================================================================
// TTL boundaries (fuzz dimension).
//
// For a Present persistent ContractCode / ContractData (instance) entry, vary its
// live_until_ledger around the current ledger sequence. Observed host behavior:
//   * live_until >= seq  -> LIVE: byte-identical to the all-live reference.
//   * live_until <  seq  -> ARCHIVED but auto-restorable in simulation: still a
//                            successful invoke (tag 0), but DIFFERENT bytes
//                            (restoration accounting adds footprint/rent) — NOT
//                            NeedsState and NOT an error.
//   * live_until == None -> the host aborts the read with (Storage, InternalError),
//                            surfaced as a Complete error branch (exercises the
//                            `Some(live_until)` requirement).
// ===========================================================================

/// Build the oracle snapshot with a single entry's TTL overridden.
/// `which` selects the entry: 0 = callee code, 1 = callee instance.
fn oracle_with_ttl(which: usize, ttl: Option<u32>) -> Vec<LedgerEntryWithLiveUntil> {
    let mut entries = vec![
        code(CALLER_CONTRACT_WASM, LIVE),
        inst(CALLER_CONTRACT_ID, CALLER_CONTRACT_WASM, LIVE),
        code(FIXTURE_CONTRACT_WASM, LIVE),
        inst(CALLEE_CONTRACT_ID, FIXTURE_CONTRACT_WASM, LIVE),
    ];
    let target = match which {
        0 => 2, // callee code
        1 => 3, // callee instance
        _ => unreachable!(),
    };
    entries[target].live_until_ledger = ttl;
    entries
}

#[test]
fn ttl_boundary_live_at_and_above_sequence_matches_reference() {
    let reference = reference_bytes();

    for which in [0usize, 1usize] {
        // live_until == seq is the last LIVE ledger; live_until == seq + 1 is live.
        for ttl in [SEQ, SEQ + 1] {
            let bytes = expect_complete(run(
                nested_hf(),
                oracle_with_ttl(which, Some(ttl)),
                vec![],
                UnmappedPolicy::Absent,
            ));
            assert_eq!(invoke_tag(&bytes), 0, "live entry -> successful invoke");
            assert_eq!(
                bytes, reference,
                "live_until >= seq is byte-identical to the all-live reference (which={which} ttl={ttl})"
            );
        }
    }
}

#[test]
fn ttl_boundary_below_sequence_still_completes_but_differs() {
    let reference = reference_bytes();

    for which in [0usize, 1usize] {
        // live_until == seq - 1: archived, but auto-restorable during simulation.
        let outcome = run(
            nested_hf(),
            oracle_with_ttl(which, Some(SEQ - 1)),
            vec![],
            UnmappedPolicy::Absent,
        );
        // A present-but-archived persistent entry is NOT needs-state: the host has
        // the entry, it just accounts for restoration.
        let bytes = expect_complete(outcome);
        assert_eq!(
            invoke_tag(&bytes),
            0,
            "archived-but-restorable persistent entry still invokes successfully (which={which})"
        );
        assert_ne!(
            bytes, reference,
            "an archived (live_until < seq) entry perturbs the output vs all-live (which={which})"
        );
    }
}

#[test]
fn ttl_none_on_present_contract_entry_aborts_with_storage_internal_error() {
    // The `Some(live_until)` requirement: a Present ContractCode with `None` TTL
    // makes the host abort the read with (Storage, InternalError), encoded inline
    // as a Complete error branch (never a success, never NeedsState).
    let outcome = run(nested_hf(), oracle_with_ttl(0, None), vec![], UnmappedPolicy::Absent);
    let bytes = expect_complete(outcome);
    assert_eq!(invoke_tag(&bytes), 1, "missing TTL must fail the invoke");
    let text = err_text(&bytes);
    assert!(
        text.contains("Storage") && text.contains("InternalError"),
        "expected (Storage, InternalError) for a None TTL on a present contract entry, got: {text}"
    );
}
