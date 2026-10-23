wit_bindgen::generate!({
    path: "wit",
    world: "simulator",
});

mod session;

use soroban_env_host::xdr::{
    AccountId, HostFunction, LedgerEntry as XdrLedgerEntry, LedgerKey, Limits, ReadXdr,
    TransactionEnvelope, WriteXdr,
};
use soroban_env_host::LedgerInfo;
use soroban_sim_core::{
    encode_trace, simulate, simulate_transaction, simulate_with_trace, AdjustmentSource,
    LedgerEntryWithLiveUntil, NetworkConfigSource, SimOutcome, SimulationRequest,
    TraceOutcome as CoreTraceOutcome, UnmappedPolicy,
};

struct Component;

/// Map the WIT `apply-rpc-adjustments` bool onto the core adjustment selector:
/// false (back-compat default) => raw metered values; true => RPC-parity.
fn adjustment_from_wit(apply_rpc_adjustments: bool) -> AdjustmentSource {
    if apply_rpc_adjustments {
        AdjustmentSource::RpcDefault
    } else {
        AdjustmentSource::None
    }
}

impl Guest for Component {
    fn run_fixture() -> Result<Vec<u8>, String> {
        soroban_sim_core::run_fixture().map_err(|e| e.to_string())
    }

    fn simulate(request: Request) -> Result<Outcome, String> {
        simulate_request(request).map_err(|e| e.to_string())
    }

    fn simulate_transaction(request: TransactionRequest) -> Result<Outcome, String> {
        simulate_transaction_request(request).map_err(|e| e.to_string())
    }

    fn simulate_trace(request: Request) -> Result<TraceOutcome, String> {
        simulate_trace_request(request).map_err(|e| e.to_string())
    }
}

fn simulate_transaction_request(request: TransactionRequest) -> anyhow::Result<Outcome> {
    let tx_envelope = TransactionEnvelope::from_xdr(request.transaction_xdr, Limits::none())?;
    let ledger_info = ledger_info_from_wit(request.ledger_context)?;
    let base_prng_seed = fixed_32(request.base_prng_seed, "base-prng-seed")?;
    let ledger_entries = request
        .ledger_entries
        .into_iter()
        .map(ledger_entry_from_wit)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let absent_keys = request
        .known_absent_keys
        .into_iter()
        .map(|key_xdr| LedgerKey::from_xdr(key_xdr, Limits::none()).map_err(anyhow::Error::from))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let outcome = simulate_transaction(
        tx_envelope,
        ledger_info,
        // Network config is loaded from the ConfigSetting ledger entries supplied
        // through `ledger-entries`; a missing one comes back as needs-state.
        NetworkConfigSource::FromLedgerEntries,
        ledger_entries,
        absent_keys,
        // Fail-closed, matching the host-function `simulate` path.
        UnmappedPolicy::Unknown,
        base_prng_seed,
        request.enable_diagnostics,
        adjustment_from_wit(request.apply_rpc_adjustments),
    )?;
    outcome_to_wit(outcome)
}

fn outcome_to_wit(outcome: SimOutcome) -> anyhow::Result<Outcome> {
    match outcome {
        SimOutcome::Complete(bytes) => Ok(Outcome::Complete(bytes)),
        SimOutcome::NeedsState(keys) => {
            let key_xdrs = keys
                .iter()
                .map(|k| k.to_xdr(Limits::none()).map_err(anyhow::Error::from))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Outcome::NeedsState(key_xdrs))
        }
    }
}

fn simulate_request(request: Request) -> anyhow::Result<Outcome> {
    let outcome = simulate(simulation_request_from_wit(request)?)?;
    outcome_to_wit(outcome)
}

/// simulate-trace shares the exact request mapping with simulate (same
/// FromLedgerEntries network config, fail-closed UnmappedPolicy::Unknown,
/// known-absent keys); only the outcome shape differs.
fn simulate_trace_request(request: Request) -> anyhow::Result<TraceOutcome> {
    let outcome = simulate_with_trace(simulation_request_from_wit(request)?)?;
    match outcome {
        CoreTraceOutcome::Complete { ssim, trace } => {
            Ok(TraceOutcome::Complete(TraceComplete {
                ssim,
                trace: encode_trace(&trace)?,
            }))
        }
        CoreTraceOutcome::NeedsState(keys) => {
            let key_xdrs = keys
                .iter()
                .map(|k| k.to_xdr(Limits::none()).map_err(anyhow::Error::from))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(TraceOutcome::NeedsState(key_xdrs))
        }
        CoreTraceOutcome::TraceUnavailable { ssim, reason } => {
            Ok(TraceOutcome::TraceUnavailable(TraceFallback { ssim, reason }))
        }
    }
}

fn simulation_request_from_wit(request: Request) -> anyhow::Result<SimulationRequest> {
    let host_function = HostFunction::from_xdr(request.host_function_xdr, Limits::none())?;
    let source_account = AccountId::from_xdr(request.source_account_xdr, Limits::none())?;
    let ledger_info = ledger_info_from_wit(request.ledger_context)?;
    let base_prng_seed = fixed_32(request.base_prng_seed, "base-prng-seed")?;
    let ledger_entries = request
        .ledger_entries
        .into_iter()
        .map(ledger_entry_from_wit)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let absent_keys = request
        .known_absent_keys
        .into_iter()
        .map(|key_xdr| LedgerKey::from_xdr(key_xdr, Limits::none()).map_err(anyhow::Error::from))
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(SimulationRequest {
        host_function,
        source_account,
        ledger_info,
        // Network config is loaded from the ConfigSetting ledger entries supplied
        // through `ledger-entries`; a missing one comes back as needs-state.
        network_config: NetworkConfigSource::FromLedgerEntries,
        ledger_entries,
        absent_keys,
        // WIT simulate is fail-closed: a key that is neither present nor known-absent
        // is Unknown, so the caller refetches it rather than observing a false absence.
        unmapped_policy: UnmappedPolicy::Unknown,
        base_prng_seed,
        enable_diagnostics: request.enable_diagnostics,
        adjustment: adjustment_from_wit(request.apply_rpc_adjustments),
    })
}

fn ledger_info_from_wit(context: LedgerContext) -> anyhow::Result<LedgerInfo> {
    Ok(LedgerInfo {
        protocol_version: context.protocol_version,
        sequence_number: context.sequence_number,
        timestamp: context.timestamp,
        network_id: fixed_32(context.network_id, "network-id")?,
        base_reserve: context.base_reserve,
        min_temp_entry_ttl: context.min_temp_entry_ttl,
        min_persistent_entry_ttl: context.min_persistent_entry_ttl,
        max_entry_ttl: context.max_entry_ttl,
    })
}

fn ledger_entry_from_wit(entry: LedgerEntry) -> anyhow::Result<LedgerEntryWithLiveUntil> {
    Ok(LedgerEntryWithLiveUntil {
        entry: XdrLedgerEntry::from_xdr(entry.entry_xdr, Limits::none())?,
        live_until_ledger: entry.live_until_ledger,
    })
}

fn fixed_32(bytes: Vec<u8>, name: &str) -> anyhow::Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("{name} must be 32 bytes, got {}", v.len()))
}

impl exports::theahaco::soroban_sim::network_session::Guest for Component {
    type Session = session::NetworkSession;
}

// Wasm only: interface exports have `#` in their symbol names, which the
// macOS linker's exported-symbols list reads as a comment, so a native cdylib
// (built for `cargo test`) cannot export them. Native tests call the impls.
#[cfg(target_arch = "wasm32")]
export!(Component);

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use soroban_sim_core::{decode_trace, TraceStepKind};

    #[test]
    fn component_fixture_matches_core_fixture() {
        let direct = soroban_sim_core::run_fixture().unwrap();
        let again = soroban_sim_core::run_fixture().unwrap();
        assert_eq!(direct, again);
    }

    fn b64(s: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode(s).unwrap()
    }

    /// Build a WIT-level `Request` from the committed local-add-i32 parity
    /// capture: real tx envelope + a full ledger snapshot (incl. the
    /// ConfigSetting entries `NetworkConfigSource::FromLedgerEntries` needs).
    fn parity_request() -> Request {
        let json: serde_json::Value = serde_json::from_str(include_str!(
            "../fixtures/parity/local-add-i32.json"
        ))
        .unwrap();
        let tx = TransactionEnvelope::from_xdr(b64(json["txXdr"].as_str().unwrap()), Limits::none())
            .unwrap();
        let (host_function, source_account) =
            soroban_sim_core::extract_invoke_host_function(&tx).unwrap();
        let lc = &json["ledgerContext"];
        let network_id: Vec<u8> = {
            let hex = lc["networkId"].as_str().unwrap();
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect()
        };
        let ledger_entries = json["snapshot"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| LedgerEntry {
                entry_xdr: b64(entry["entryXdr"].as_str().unwrap()),
                live_until_ledger: entry["liveUntilLedger"].as_u64().map(|v| v as u32),
            })
            .collect();
        Request {
            host_function_xdr: host_function.to_xdr(Limits::none()).unwrap(),
            source_account_xdr: source_account.to_xdr(Limits::none()).unwrap(),
            ledger_context: LedgerContext {
                // The committed local fixture can come from a neighbouring
                // protocol; execution must always use this compiled host's
                // exact interface version.
                protocol_version: soroban_env_host::meta::INTERFACE_VERSION.protocol,
                sequence_number: lc["sequenceNumber"].as_u64().unwrap() as u32,
                timestamp: lc["timestamp"].as_u64().unwrap(),
                network_id,
                base_reserve: lc["baseReserve"].as_u64().unwrap() as u32,
                min_temp_entry_ttl: lc["minTempEntryTtl"].as_u64().unwrap() as u32,
                min_persistent_entry_ttl: lc["minPersistentEntryTtl"].as_u64().unwrap() as u32,
                max_entry_ttl: lc["maxEntryTtl"].as_u64().unwrap() as u32,
            },
            ledger_entries,
            known_absent_keys: vec![],
            base_prng_seed: vec![0u8; 32],
            enable_diagnostics: true,
            apply_rpc_adjustments: false,
        }
    }

    /// simulate-trace over the full parity snapshot: trace replay completes,
    /// the STRC bytes decode, the steps are Begin..End bracketed, and the ssim
    /// bytes are identical to the plain simulate export's.
    #[test]
    fn simulate_trace_completes_on_parity_fixture() {
        let plain = match simulate_request(parity_request()).unwrap() {
            Outcome::Complete(bytes) => bytes,
            Outcome::NeedsState(keys) => panic!("plain simulate needs {} keys", keys.len()),
        };
        let traced = simulate_trace_request(parity_request()).unwrap();
        let TraceOutcome::Complete(complete) = traced else {
            panic!("expected trace-complete, got {traced:?}");
        };
        assert_eq!(complete.ssim, plain, "trace ssim must match plain simulate");
        let steps = decode_trace(&complete.trace).unwrap();
        assert!(steps.len() >= 4, "expected at least Begin/push/pop/End");
        assert_eq!(steps.first().unwrap().kind, TraceStepKind::Begin);
        assert_eq!(steps.last().unwrap().kind, TraceStepKind::End);
        assert!(
            steps
                .iter()
                .any(|s| s.kind == TraceStepKind::PushCtx && s.name.ends_with(":add")),
            "expected a VM frame push for `add`, got {:?}",
            steps.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    /// Without the ConfigSetting ledger entries, FromLedgerEntries cannot load
    /// the network config: the trace path must surface needs-state (same
    /// fail-closed contract as plain simulate), not an error.
    #[test]
    fn simulate_trace_needs_state_without_config_entries() {
        let mut request = parity_request();
        request.ledger_entries.retain(|entry| {
            let parsed =
                XdrLedgerEntry::from_xdr(entry.entry_xdr.clone(), Limits::none()).unwrap();
            !matches!(
                parsed.data,
                soroban_env_host::xdr::LedgerEntryData::ConfigSetting(_)
            )
        });
        let traced = simulate_trace_request(request).unwrap();
        let TraceOutcome::NeedsState(keys) = traced else {
            panic!("expected needs-state, got {traced:?}");
        };
        assert!(!keys.is_empty());
    }
}
