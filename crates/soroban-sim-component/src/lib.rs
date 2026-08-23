wit_bindgen::generate!({
    path: "../../wit",
    world: "simulator",
});

use soroban_env_host::xdr::{
    AccountId, HostFunction, LedgerEntry as XdrLedgerEntry, LedgerKey, Limits, ReadXdr,
    TransactionEnvelope, WriteXdr,
};
use soroban_env_host::LedgerInfo;
use soroban_sim_core::{
    simulate, simulate_transaction, AdjustmentSource, LedgerEntryWithLiveUntil,
    NetworkConfigSource, SimOutcome, SimulationRequest, UnmappedPolicy,
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

    let outcome = simulate(SimulationRequest {
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
    })?;
    outcome_to_wit(outcome)
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

export!(Component);

#[cfg(test)]
mod tests {
    #[test]
    fn component_fixture_matches_core_fixture() {
        let direct = soroban_sim_core::run_fixture().unwrap();
        let again = soroban_sim_core::run_fixture().unwrap();
        assert_eq!(direct, again);
    }
}
