//! The only enforcing-invocation ABI difference between the supported hosts.
use super::host;
use host::e2e_invoke;
#[cfg(feature = "host-p27")]
use host::{budget::Budget, xdr::DiagnosticEvent, HostError, LedgerInfo, ModuleCache, TraceHook};

#[cfg(not(feature = "host-p27"))]
pub use e2e_invoke::{invoke_host_function, TtlLedgerEntryMeta};

#[cfg(feature = "host-p27")]
pub struct TtlLedgerEntryMeta {
    pub live_until_ledger: u32,
    #[allow(dead_code)] // Protocol 27 recomputes this inside the host.
    pub entry_size_for_rent: u32,
}

#[cfg(feature = "host-p27")]
#[allow(clippy::too_many_arguments)]
pub fn invoke_host_function(
    budget: &Budget,
    enable_diagnostics: bool,
    encoded_host_fn: Vec<u8>,
    encoded_resources: Vec<u8>,
    restored: &[u32],
    source: Vec<u8>,
    auth: impl ExactSizeIterator<Item = Vec<u8>>,
    info: LedgerInfo,
    entries: impl ExactSizeIterator<Item = (Option<Vec<u8>>, Option<TtlLedgerEntryMeta>)>,
    seed: Vec<u8>,
    diagnostics: &mut Vec<DiagnosticEvent>,
    hook: Option<TraceHook>,
    cache: Option<ModuleCache>,
) -> Result<e2e_invoke::InvokeHostFunctionResult, HostError> {
    use host::xdr::{Hash, Limits, ReadXdr, SorobanResources, TtlEntry, WriteXdr};
    use sha2::{Digest, Sha256};
    let invalid = |_| {
        HostError::from(host::Error::from_type_and_code(
            host::xdr::ScErrorType::Value,
            host::xdr::ScErrorCode::InvalidInput,
        ))
    };
    let resources =
        SorobanResources::from_xdr(&encoded_resources, Limits::none()).map_err(invalid)?;
    let keys = resources
        .footprint
        .read_only
        .iter()
        .chain(resources.footprint.read_write.iter());
    let mut ledger = vec![];
    let mut ttl = vec![];
    for (key, (entry, meta)) in keys.zip(entries) {
        // p27 takes only present entries, with a parallel encoded TTL vector.
        if let Some(entry) = entry {
            ledger.push(entry);
            ttl.push(match meta {
                Some(meta) => TtlEntry {
                    key_hash: Hash(
                        Sha256::digest(key.to_xdr(Limits::none()).map_err(invalid)?).into(),
                    ),
                    live_until_ledger_seq: meta.live_until_ledger,
                }
                .to_xdr(Limits::none())
                .map_err(invalid)?,
                None => vec![],
            });
        }
    }
    e2e_invoke::invoke_host_function(
        budget,
        enable_diagnostics,
        encoded_host_fn,
        encoded_resources,
        restored,
        source,
        auth.collect::<Vec<_>>().into_iter(),
        info,
        ledger.into_iter(),
        ttl.into_iter(),
        seed,
        diagnostics,
        hook,
        cache,
    )
}
