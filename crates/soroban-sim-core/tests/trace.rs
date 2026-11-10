//! Trace-pass acceptance tests: two-pass `simulate_with_trace` over the
//! fixture contracts.
//!
//! (a) add-i32 complete run -> `TraceOutcome::Complete` with a well-formed
//!     Begin..End trace (>=1 PushCtx/PopCtx pair, >=1 EnvCall, monotonic cpu);
//! (b) determinism: two runs -> byte-identical encoded traces;
//! (c) the SSIM bytes from `simulate_with_trace` equal `simulate`'s exactly;
//! (d) nested caller fixture -> >=2 PushCtx (cross-contract frame visible);
//! (e) storage get_persist -> an EnvCall naming the storage host fn.

use soroban_sim_core::host::xdr::{ContractDataDurability, Limits, ReadXdr, ScVal};
use soroban_sim_core::*;

/// A live-until comfortably in the future (> fixture sequence 1_000_000).
const LIVE: u32 = 1_500_000;

fn p(entry: soroban_sim_core::host::xdr::LedgerEntry, ttl: u32) -> LedgerEntryWithLiveUntil {
    LedgerEntryWithLiveUntil {
        entry,
        live_until_ledger: Some(ttl),
    }
}

fn expect_complete(outcome: TraceOutcome) -> (Vec<u8>, Vec<TraceStep>) {
    match outcome {
        TraceOutcome::Complete { ssim, trace } => (ssim, trace),
        TraceOutcome::NeedsState(keys) => panic!("expected Complete, got NeedsState {keys:?}"),
        TraceOutcome::TraceUnavailable { reason, .. } => {
            panic!("expected Complete, got TraceUnavailable: {reason}")
        }
    }
}

/// Decode the Ok `ScVal` from the SSIM bytes (asserts the Ok branch).
/// Layout: "SSIM0\0" (6) + instructions u32 (4) + memory u32 (4) + tag byte.
fn ok_scval(bytes: &[u8]) -> ScVal {
    let mut o = 6 + 4 + 4;
    assert_eq!(bytes[o], 0, "expected Ok invoke_result branch");
    o += 1;
    let len = u32::from_be_bytes(bytes[o..o + 4].try_into().unwrap()) as usize;
    o += 4;
    ScVal::from_xdr(&bytes[o..o + len], Limits::none()).unwrap()
}

fn count_kind(trace: &[TraceStep], kind: TraceStepKind) -> usize {
    trace.iter().filter(|s| s.kind == kind).count()
}

// ===========================================================================
// (a) add-i32 complete run: well-formed trace
// ===========================================================================

#[test]
fn fixture_trace_is_well_formed() {
    let outcome = simulate_with_trace(fixture_request().unwrap()).unwrap();
    let (ssim, trace) = expect_complete(outcome);

    // The SSIM still carries the real result.
    assert_eq!(ok_scval(&ssim), ScVal::I32(FIXTURE_SUM));

    // Begin .. End bracket the trace.
    assert!(trace.len() >= 4, "trace too short: {} steps", trace.len());
    assert_eq!(trace.first().unwrap().kind, TraceStepKind::Begin);
    assert_eq!(trace.last().unwrap().kind, TraceStepKind::End);
    assert_eq!(count_kind(&trace, TraceStepKind::Begin), 1);
    assert_eq!(count_kind(&trace, TraceStepKind::End), 1);

    // At least one contract frame push/pop pair, balanced.
    let pushes = count_kind(&trace, TraceStepKind::PushCtx);
    let pops = count_kind(&trace, TraceStepKind::PopCtx);
    assert!(pushes >= 1, "expected >=1 PushCtx, got {pushes}");
    assert_eq!(pushes, pops, "unbalanced frame pushes/pops");

    // The add-i32 VM frame is visible with its args in the Display rendering.
    // (The first PushCtx is the `InvokeContract` host-function wrapper frame;
    // the contract's own VM frame follows it.)
    let push = trace
        .iter()
        .find(|s| s.kind == TraceStepKind::PushCtx && s.name.starts_with("VM:"))
        .unwrap_or_else(|| {
            panic!(
                "no VM frame push; pushes seen: {:?}",
                trace
                    .iter()
                    .filter(|s| s.kind == TraceStepKind::PushCtx)
                    .map(|s| &s.name)
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        push.name.ends_with(":add"),
        "unexpected frame label: {}",
        push.name
    );
    assert!(
        push.display.starts_with("push ") && push.display.contains("add"),
        "unexpected push display: {}",
        push.display
    );

    // NOTE: add-i32 is pure wasm arithmetic — it never dispatches a VM->host
    // env function, so its trace legitimately has ZERO EnvCall steps (only
    // Begin / frame push+pop / End). EnvCall coverage lives in the storage and
    // caller fixture tests below, whose contracts do dispatch env fns.
    for step in trace.iter().filter(|s| s.kind == TraceStepKind::EnvCall) {
        assert!(!step.name.is_empty());
        assert!(
            step.display.starts_with("call "),
            "unexpected EnvCall display: {}",
            step.display
        );
    }

    // seq is the index; cpu is monotonically non-decreasing.
    for (i, step) in trace.iter().enumerate() {
        assert_eq!(step.seq as usize, i);
        if i > 0 {
            assert!(
                step.cpu >= trace[i - 1].cpu,
                "cpu regressed at step {i}: {} < {}",
                step.cpu,
                trace[i - 1].cpu
            );
        }
    }
    // Execution consumed budget: the final sample is positive.
    assert!(trace.last().unwrap().cpu > 0);
}

// ===========================================================================
// (b) determinism: identical encoded trace bytes across runs
// ===========================================================================

#[test]
fn trace_is_deterministic_across_runs() {
    let (_, first) = expect_complete(simulate_with_trace(fixture_request().unwrap()).unwrap());
    let (_, second) = expect_complete(simulate_with_trace(fixture_request().unwrap()).unwrap());

    let first_bytes = encode_trace(&first).unwrap();
    let second_bytes = encode_trace(&second).unwrap();
    assert_eq!(first_bytes, second_bytes, "encoded traces differ across runs");

    // And the encoding round-trips.
    assert_eq!(decode_trace(&first_bytes).unwrap(), first);
}

// ===========================================================================
// (c) SSIM parity with plain simulate()
// ===========================================================================

#[test]
fn trace_ssim_matches_plain_simulate_bytes() {
    let (ssim, _) = expect_complete(simulate_with_trace(fixture_request().unwrap()).unwrap());
    let plain = match simulate(fixture_request().unwrap()).unwrap() {
        SimOutcome::Complete(bytes) => bytes,
        SimOutcome::NeedsState(keys) => panic!("fixture needs state: {keys:?}"),
    };
    assert_eq!(ssim, plain, "simulate_with_trace SSIM must be byte-identical");

    // The golden fixture path is untouched too.
    assert_eq!(plain, run_fixture().unwrap());
}

// ===========================================================================
// (d) nested caller fixture: cross-contract frames visible
// ===========================================================================

#[test]
fn nested_call_shows_both_contract_frames() {
    let request = integration_request(
        caller_host_function(CALLEE_CONTRACT_ID, 40, 2).unwrap(),
        vec![
            p(contract_code_entry(CALLER_CONTRACT_WASM).unwrap(), LIVE),
            p(
                contract_instance_entry(CALLER_CONTRACT_ID, CALLER_CONTRACT_WASM, None).unwrap(),
                LIVE,
            ),
            p(contract_code_entry(FIXTURE_CONTRACT_WASM).unwrap(), LIVE),
            p(
                contract_instance_entry(CALLEE_CONTRACT_ID, FIXTURE_CONTRACT_WASM, None).unwrap(),
                LIVE,
            ),
        ],
        vec![],
        UnmappedPolicy::Absent,
    );
    let (ssim, trace) = expect_complete(simulate_with_trace(request).unwrap());
    assert_eq!(ok_scval(&ssim), ScVal::I32(42));

    let pushes: Vec<&TraceStep> = trace
        .iter()
        .filter(|s| s.kind == TraceStepKind::PushCtx)
        .collect();
    assert!(
        pushes.len() >= 2,
        "expected >=2 PushCtx for a cross-contract call, got {}",
        pushes.len()
    );
    // Two distinct frames: the caller's call_add and the callee's add.
    assert!(
        pushes.iter().any(|s| s.name.ends_with(":call_add")),
        "caller frame missing: {:?}",
        pushes.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        pushes.iter().any(|s| s.name.ends_with(":add")),
        "callee frame missing: {:?}",
        pushes.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert_eq!(
        pushes.len(),
        count_kind(&trace, TraceStepKind::PopCtx),
        "unbalanced frames"
    );

    // The cross-contract invocation itself dispatches env host fns (`call`,
    // plus symbol/address conversions), so EnvCall steps must be present here.
    assert!(
        count_kind(&trace, TraceStepKind::EnvCall) >= 1,
        "expected >=1 EnvCall in the nested-call trace"
    );
    assert!(
        trace
            .iter()
            .any(|s| s.kind == TraceStepKind::EnvCall && s.name == "call"),
        "expected the cross-contract `call` env fn; EnvCalls seen: {:?}",
        trace
            .iter()
            .filter(|s| s.kind == TraceStepKind::EnvCall)
            .map(|s| &s.name)
            .collect::<Vec<_>>()
    );
}

// ===========================================================================
// (e) storage fixture: storage host fn visible as an EnvCall
// ===========================================================================

#[test]
fn storage_read_shows_storage_host_fn_env_call() {
    let request = integration_request(
        storage_host_function("get_persist", vec![ScVal::U32(7)]).unwrap(),
        vec![
            p(contract_code_entry(STORAGE_CONTRACT_WASM).unwrap(), LIVE),
            p(
                contract_instance_entry(STORAGE_CONTRACT_ID, STORAGE_CONTRACT_WASM, None).unwrap(),
                LIVE,
            ),
            p(
                contract_data_entry(
                    STORAGE_CONTRACT_ID,
                    ScVal::U32(7),
                    ContractDataDurability::Persistent,
                    ScVal::U32(99),
                ),
                LIVE,
            ),
        ],
        vec![],
        UnmappedPolicy::Absent,
    );
    let (ssim, trace) = expect_complete(simulate_with_trace(request).unwrap());
    assert_eq!(ok_scval(&ssim), ScVal::U32(99));

    let storage_call = trace
        .iter()
        .find(|s| s.kind == TraceStepKind::EnvCall && s.name.contains("contract_data"));
    let step = storage_call.unwrap_or_else(|| {
        panic!(
            "no EnvCall naming a *contract_data* host fn; EnvCalls seen: {:?}",
            trace
                .iter()
                .filter(|s| s.kind == TraceStepKind::EnvCall)
                .map(|s| &s.name)
                .collect::<Vec<_>>()
        )
    });
    assert!(step.display.contains("contract_data"), "display: {}", step.display);

    // Its EnvRet counterpart is present too.
    assert!(trace
        .iter()
        .any(|s| s.kind == TraceStepKind::EnvRet && s.name == step.name));
}
