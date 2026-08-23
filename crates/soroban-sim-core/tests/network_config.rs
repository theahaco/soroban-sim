//! M1 network-config sourcing tests (closes the `FromLedgerEntries` gap).
//!
//! Two things are proven here:
//!
//! 1. `NetworkConfigSource::FromLedgerEntries` reconstructs the `NetworkConfig`
//!    out of the `ConfigSetting` ledger entries in the snapshot and produces a
//!    simulation that is **byte-identical** to hand-providing
//!    `fixture_network_config()` via `NetworkConfigSource::Explicit`. This
//!    exercises `NetworkConfig::load_from_snapshot`, which no other test drives.
//!
//! 2. A genuine resource-limit failure — a `(Budget, ExceededLimit)` from an
//!    absurdly low `tx_max_instructions` — is classified as `SimOutcome::Complete`
//!    (error branch encoded inline), NOT `SimOutcome::NeedsState`. This is the
//!    sentinel-collision hardening case: only `(Storage, ExceededLimit)` recorded
//!    into the needs-state buffer means needs-state, and the buffer is
//!    authoritative, so a non-Storage `ExceededLimit` must not false-positive.

use soroban_env_host::xdr::{
    ConfigSettingContractBandwidthV0, ConfigSettingContractComputeV0, ConfigSettingContractEventsV0,
    ConfigSettingContractHistoricalDataV0, ConfigSettingContractLedgerCostExtV0,
    ConfigSettingContractLedgerCostV0, ConfigSettingEntry, LedgerEntry, LedgerEntryData,
    LedgerEntryExt, Limits, ReadXdr, ScVal, StateArchivalSettings,
};
use soroban_sim_core::*;

/// A live-until comfortably in the future (> fixture sequence, < max_entry_ttl).
const LIVE: u32 = 1_500_000;

// --- SSIM output decoders (layout: "SSIM0\0" (6) + insns u32 (4) + mem u32 (4)
//     = offset 14, then the invoke-result tag byte). -------------------------

/// Invoke-result discriminant byte: 0 = Ok, 1 = contract/host failure.
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

// --- request / entry builders ----------------------------------------------

fn present(entry: LedgerEntry, ttl: Option<u32>) -> LedgerEntryWithLiveUntil {
    LedgerEntryWithLiveUntil {
        entry,
        live_until_ledger: ttl,
    }
}

/// A Present `ConfigSetting` ledger entry. Config settings carry no TTL, and
/// `load_from_snapshot` ignores the live-until, so it is left `None`.
fn config_setting(cs: ConfigSettingEntry) -> LedgerEntryWithLiveUntil {
    present(
        LedgerEntry {
            last_modified_ledger_seq: 0,
            data: LedgerEntryData::ConfigSetting(cs),
            ext: LedgerEntryExt::V0,
        },
        None,
    )
}

/// The fixture add_i32 contract code + instance entries.
fn code_and_instance() -> Vec<LedgerEntryWithLiveUntil> {
    vec![
        present(fixture_contract_code_entry().unwrap(), Some(LIVE)),
        present(fixture_contract_instance_entry().unwrap(), Some(LIVE)),
    ]
}

/// The full set of `ConfigSetting` ledger entries that
/// `NetworkConfig::load_from_snapshot` reads, populated with the SAME fee/cost
/// numbers as `fixture_network_config()` so the reconstructed config matches.
///
/// The only field `load_from_snapshot` cannot reproduce is
/// `rent_fee_configuration.fee_per_rent_1kb`: it is *computed* via
/// `compute_rent_write_fee_per_1kb` and floored at
/// `MINIMUM_RENT_WRITE_FEE_PER_1KB` (1000), whereas the fixture hard-codes 100.
/// The rent-write params below are chosen so the computed value is the 1000
/// floor. For the read-only add_i32 invocation there are no writes and no TTL
/// bumps, so rent fees are zero and `fee_per_rent_1kb` never enters the fee
/// computation — hence the byte-identity assertion still holds (see the test).
fn fixture_config_setting_entries() -> Vec<LedgerEntryWithLiveUntil> {
    // Reuse the fixture's exact cost-param vectors so the metered CPU / memory
    // (which DO affect the output) are guaranteed identical.
    let fixture = fixture_network_config();

    let compute = ConfigSettingEntry::ContractComputeV0(ConfigSettingContractComputeV0 {
        ledger_max_instructions: 0, // unused by load_from_snapshot
        tx_max_instructions: 100_000_000,
        fee_rate_per_instructions_increment: 10,
        tx_memory_limit: 40_000_000,
    });

    let ledger_cost = ConfigSettingEntry::ContractLedgerCostV0(ConfigSettingContractLedgerCostV0 {
        // The 8 "max" fields are unused by load_from_snapshot.
        ledger_max_disk_read_entries: 0,
        ledger_max_disk_read_bytes: 0,
        ledger_max_write_ledger_entries: 0,
        ledger_max_write_bytes: 0,
        tx_max_disk_read_entries: 0,
        tx_max_disk_read_bytes: 0,
        tx_max_write_ledger_entries: 0,
        tx_max_write_bytes: 0,
        fee_disk_read_ledger_entry: 20,
        fee_write_ledger_entry: 30,
        fee_disk_read1_kb: 40,
        // Rent-write params: chosen so compute_rent_write_fee_per_1kb hits the
        // 1000 floor. Irrelevant to the read-only add_i32 output.
        soroban_state_target_size_bytes: 1_000_000,
        rent_fee1_kb_soroban_state_size_low: 100,
        rent_fee1_kb_soroban_state_size_high: 100,
        soroban_state_rent_fee_growth_factor: 1,
    });

    let ledger_cost_ext =
        ConfigSettingEntry::ContractLedgerCostExtV0(ConfigSettingContractLedgerCostExtV0 {
            tx_max_footprint_entries: 0, // unused by load_from_snapshot
            fee_write1_kb: 50,
        });

    let historical =
        ConfigSettingEntry::ContractHistoricalDataV0(ConfigSettingContractHistoricalDataV0 {
            fee_historical1_kb: 60,
        });

    let events = ConfigSettingEntry::ContractEventsV0(ConfigSettingContractEventsV0 {
        tx_max_contract_events_size_bytes: 0, // unused by load_from_snapshot
        fee_contract_events1_kb: 70,
    });

    let bandwidth = ConfigSettingEntry::ContractBandwidthV0(ConfigSettingContractBandwidthV0 {
        ledger_max_txs_size_bytes: 0, // unused by load_from_snapshot
        tx_max_size_bytes: 0,         // unused by load_from_snapshot
        fee_tx_size1_kb: 80,
    });

    let state_archival = ConfigSettingEntry::StateArchival(StateArchivalSettings {
        max_entry_ttl: 10_000_000,
        min_temporary_ttl: 16,
        min_persistent_ttl: 100_000,
        persistent_rent_rate_denominator: 100,
        temp_rent_rate_denominator: 1000,
        // The remaining fields are unused by load_from_snapshot.
        max_entries_to_archive: 0,
        live_soroban_state_size_window_sample_size: 0,
        live_soroban_state_size_window_sample_period: 0,
        eviction_scan_size: 0,
        starting_eviction_scan_level: 0,
    });

    let cpu = ConfigSettingEntry::ContractCostParamsCpuInstructions(fixture.cpu_cost_params.clone());
    let memory = ConfigSettingEntry::ContractCostParamsMemoryBytes(fixture.memory_cost_params.clone());

    // Non-empty state-size window (a single 0 → average bucket-list size 0).
    let window =
        ConfigSettingEntry::LiveSorobanStateSizeWindow(vec![0_u64].try_into().unwrap());

    [
        compute,
        ledger_cost,
        ledger_cost_ext,
        historical,
        events,
        bandwidth,
        state_archival,
        cpu,
        memory,
        window,
    ]
    .into_iter()
    .map(config_setting)
    .collect()
}

/// A [`SimulationRequest`] over the fixture add_i32 invocation, parameterised by
/// the network-config source, ledger entries, and unmapped policy.
fn add_i32_request(
    network_config: NetworkConfigSource,
    ledger_entries: Vec<LedgerEntryWithLiveUntil>,
    unmapped_policy: UnmappedPolicy,
) -> SimulationRequest {
    SimulationRequest {
        host_function: fixture_host_function().unwrap(),
        source_account: account_id(FIXTURE_SOURCE_ACCOUNT),
        ledger_info: fixture_ledger_info(),
        network_config,
        ledger_entries,
        absent_keys: vec![],
        unmapped_policy,
        base_prng_seed: FIXTURE_PRNG_SEED,
        enable_diagnostics: true,
    }
}

// ===========================================================================
// Task 1: FromLedgerEntries drives load_from_snapshot and is byte-identical to
//         the Explicit(fixture_network_config()) run.
// ===========================================================================

#[test]
fn from_ledger_entries_matches_explicit_fixture_byte_for_byte() {
    // Explicit baseline: caller hands over fixture_network_config() directly.
    let explicit = expect_complete(
        simulate(add_i32_request(
            NetworkConfigSource::Explicit(fixture_network_config()),
            code_and_instance(),
            UnmappedPolicy::Absent,
        ))
        .unwrap(),
    );

    // FromLedgerEntries: the network config is rebuilt from ConfigSetting entries
    // carrying the same numbers. code + instance are present too so the invoke
    // itself resolves; Absent policy so nothing surfaces as needs-state.
    let mut entries = code_and_instance();
    entries.extend(fixture_config_setting_entries());
    let from_ledger = expect_complete(
        simulate(add_i32_request(
            NetworkConfigSource::FromLedgerEntries,
            entries,
            UnmappedPolicy::Absent,
        ))
        .unwrap(),
    );

    // Both invocations return add_i32(40, 2) == 42.
    assert_eq!(ok_scval(&explicit), ScVal::I32(FIXTURE_SUM));
    assert_eq!(ok_scval(&from_ledger), ScVal::I32(FIXTURE_SUM));

    // STRONGEST: byte-identical SSIM output. Loading the network config from the
    // ConfigSetting ledger entries yields exactly the same simulation as
    // hand-providing fixture_network_config().
    assert_eq!(
        from_ledger, explicit,
        "FromLedgerEntries must be byte-identical to Explicit(fixture_network_config())"
    );
}

// ===========================================================================
// Task 2: a genuine (Budget, ExceededLimit) is Complete(error branch), never
//         NeedsState — the needs-state buffer stayed empty.
// ===========================================================================

#[test]
fn budget_exhaustion_is_complete_not_needs_state() {
    // Fixture config, but with an absurdly low CPU limit that forces the host to
    // abort with (Budget, ExceededLimit) partway through the invocation.
    let mut cfg = fixture_network_config();
    cfg.tx_max_instructions = 100;

    // Complete snapshot (code + instance present) under Absent policy, so NO key
    // is ever read while Unknown: the needs-state buffer stays empty. This is the
    // collision case — the host raises `ExceededLimit`, but of type Budget, not
    // Storage, and the empty buffer must keep it classified as Complete.
    let outcome = simulate(add_i32_request(
        NetworkConfigSource::Explicit(cfg),
        code_and_instance(),
        UnmappedPolicy::Absent,
    ))
    .unwrap();

    let bytes = match outcome {
        SimOutcome::Complete(b) => b,
        SimOutcome::NeedsState(keys) => {
            panic!("budget exhaustion misclassified as NeedsState: {keys:?}")
        }
    };

    // Error branch (tag 1), not a successful ScVal.
    assert_eq!(
        invoke_tag(&bytes),
        1,
        "budget exhaustion must encode as the Complete error branch, not success"
    );

    // ...and it is specifically a (Budget, ExceededLimit) host error, proving the
    // buffer-authoritative classifier does not treat a non-Storage ExceededLimit
    // as the needs-state sentinel.
    let text = err_text(&bytes);
    assert!(
        text.contains("Budget") && text.contains("ExceededLimit"),
        "expected a (Budget, ExceededLimit) host error, got: {text}"
    );
}
