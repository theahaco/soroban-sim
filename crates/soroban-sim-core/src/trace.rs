//! Pass-2 trace replay: re-execute a completed pass-1 simulation in enforcing
//! mode with a [`soroban_env_host::TraceHook`] installed, producing a
//! semantic-step trace (frame pushes/pops + every env host-fn call/return)
//! with cumulative budget samples per step.
//!
//! Two-pass architecture:
//!   * Pass 1 (recording) is the existing [`crate::simulate`] internals — it
//!     records the footprint/resources/auth into
//!     `InvokeHostFunctionSimulationResult`.
//!   * Pass 2 replays via `soroban_env_host::e2e_invoke::invoke_host_function`
//!     (the one public seam that accepts a `TraceHook`) using the pass-1
//!     resources/footprint/auth, the same ledger info and PRNG seed, and
//!     ledger entries fetched from the same `ThreeStateSnapshot` — so the
//!     replay is deterministically identical to the recording.
//!
//! Replay is best-effort: any pass-2 failure degrades to
//! [`TraceOutcome::TraceUnavailable`] carrying the pass-1 SSIM bytes so
//! callers can fall back to diagnostics-only. Known unavoidable case:
//! recorded ADDRESS-credential auth has its emulated signatures blanked by
//! simulation, so enforcing replay fails signature checks; SOURCE-ACCOUNT
//! auth (the recording default here) replays fine.

use anyhow::{anyhow, Result};
use soroban_env_host::budget::{AsBudget, Budget};
use soroban_env_host::e2e_invoke;
use crate::host;
#[path = "host_adapter.rs"]
#[allow(clippy::duplicate_mod)] // Each instance binds to a different host crate identity.
mod adapter;
use adapter::TtlLedgerEntryMeta;
use soroban_env_host::storage::SnapshotSource;
use soroban_env_host::xdr::{
    DiagnosticEvent, LedgerEntryData, LedgerKey, Limits, ReadXdr, ScVal,
    SorobanTransactionDataExt, WriteXdr,
};
use soroban_env_host::{TraceEvent, TraceHook, DEFAULT_XDR_RW_LIMITS};
use std::cell::RefCell;
use std::rc::Rc;

use crate::{
    encode_simulation_result, simulate_core, CoreOutcome, SimulationCore, SimulationRequest,
};

/// Shadow-limit factors replicated from soroban-simulation's private
/// `NetworkConfig::create_budget` (network_config.rs in 29.0.2), so the
/// pass-2 enforcing budget matches the pass-1 recording budget exactly.
/// (`create_budget` is `pub(crate)` there, so we rebuild the same budget via
/// the public `Budget::try_from_configs_with_shadow_limits`.)
const CPU_SHADOW_LIMIT_FACTOR: u64 = 10;
const MEMORY_SHADOW_LIMIT_FACTOR: u64 = 2;

/// The kind of a semantic trace step, mirroring the
/// [`soroban_env_host::TraceEvent`] variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceStepKind {
    /// Host execution began (first event of every trace).
    Begin,
    /// A contract frame was pushed (contract invocation entered).
    PushCtx,
    /// A contract frame was popped (contract invocation returned).
    PopCtx,
    /// A VM->host env function was called (storage/vec/map/call/crypto/log/...).
    EnvCall,
    /// A VM->host env function returned.
    EnvRet,
    /// Host execution ended (last event of every trace).
    End,
}

impl TraceStepKind {
    fn to_u8(self) -> u8 {
        match self {
            TraceStepKind::Begin => 0,
            TraceStepKind::PushCtx => 1,
            TraceStepKind::PopCtx => 2,
            TraceStepKind::EnvCall => 3,
            TraceStepKind::EnvRet => 4,
            TraceStepKind::End => 5,
        }
    }

    fn from_u8(byte: u8) -> Result<Self> {
        Ok(match byte {
            0 => TraceStepKind::Begin,
            1 => TraceStepKind::PushCtx,
            2 => TraceStepKind::PopCtx,
            3 => TraceStepKind::EnvCall,
            4 => TraceStepKind::EnvRet,
            5 => TraceStepKind::End,
            other => return Err(anyhow!("invalid TraceStepKind byte: {other}")),
        })
    }
}

/// One semantic step of a traced execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceStep {
    /// 0-based position of this step in the trace.
    pub seq: u32,
    /// Which `TraceEvent` variant produced this step.
    pub kind: TraceStepKind,
    /// Short name: the env fn name for EnvCall/EnvRet (e.g.
    /// `get_contract_data`), the frame label for PushCtx/PopCtx (e.g.
    /// `VM:c288faf9:add`), `begin`/`end` for Begin/End.
    pub name: String,
    /// The event's full public `Display` rendering, e.g.
    /// `push VM:c288faf9:add(I32(40), I32(2))` / `call map_get(...)` /
    /// `ret get_contract_data -> Ok(...)`.
    pub display: String,
    /// Cumulative CPU instructions consumed at the time of the event.
    pub cpu: u64,
    /// Cumulative memory bytes consumed at the time of the event.
    pub mem: u64,
}

/// Outcome of [`simulate_with_trace`].
#[derive(Debug)]
pub enum TraceOutcome {
    /// Pass 1 completed and the enforcing replay reproduced it exactly:
    /// `ssim` is byte-identical to what [`crate::simulate`] returns, `trace`
    /// is the recorded step list (Begin .. End).
    Complete { ssim: Vec<u8>, trace: Vec<TraceStep> },
    /// Pass 1 read keys that are Unknown; refetch these and rerun.
    NeedsState(Vec<LedgerKey>),
    /// Pass 1 completed but the replay could not run or did not reproduce the
    /// recorded result; callers degrade to diagnostics-only over `ssim`.
    TraceUnavailable { ssim: Vec<u8>, reason: String },
}

/// Two-pass traced simulation. Pass 1 is byte-identical to [`crate::simulate`]
/// (same snapshot/needs-state semantics, same SSIM encoding); pass 2 replays
/// the recorded execution in enforcing mode with a trace hook installed.
pub fn simulate_with_trace(request: SimulationRequest) -> Result<TraceOutcome> {
    let core = match simulate_core(request)? {
        CoreOutcome::NeedsState(keys) => return Ok(TraceOutcome::NeedsState(keys)),
        CoreOutcome::Complete(core) => core,
    };
    let ssim = encode_simulation_result(&core.result)?;
    match replay_with_trace(&core) {
        Ok(trace) => Ok(TraceOutcome::Complete { ssim, trace }),
        Err(reason) => Ok(TraceOutcome::TraceUnavailable { ssim, reason }),
    }
}

/// Enforcing replay (pass 2). All failures are returned as human-readable
/// reasons; the caller maps them to [`TraceOutcome::TraceUnavailable`].
fn replay_with_trace(core: &SimulationCore) -> std::result::Result<Vec<TraceStep>, String> {
    // A failed pass-1 invocation has no transaction_data (soroban-simulation
    // sets it to None), hence no recorded footprint/resources to replay over.
    let tx_data = core.result.transaction_data.as_ref().ok_or_else(|| {
        format!(
            "pass-1 invocation failed ({:?}); no recorded resources/footprint to replay",
            core.result.invoke_result.as_ref().err()
        )
    })?;
    let resources = &tx_data.resources;
    let restored_rw_entry_indices: Vec<u32> = match &tx_data.ext {
        SorobanTransactionDataExt::V0 => vec![],
        SorobanTransactionDataExt::V1(ext) => ext.archived_soroban_entries.iter().copied().collect(),
    };

    // Same budget soroban-simulation builds for the recording pass.
    let budget = Budget::try_from_configs_with_shadow_limits(
        core.network_config.tx_max_instructions as u64,
        core.network_config.tx_memory_limit as u64,
        (core.network_config.tx_max_instructions as u64).saturating_mul(CPU_SHADOW_LIMIT_FACTOR),
        (core.network_config.tx_memory_limit as u64).saturating_mul(MEMORY_SHADOW_LIMIT_FACTOR),
        core.network_config.cpu_cost_params.clone(),
        core.network_config.memory_cost_params.clone(),
    )
    .map_err(|e| format!("building replay budget failed: {e:?}"))?;

    let encoded_host_fn = xdr_bytes(&core.host_function)?;
    let encoded_resources = xdr_bytes(resources)?;
    let encoded_source_account = xdr_bytes(&core.source_account)?;
    let encoded_auth_entries = core
        .result
        .auth
        .iter()
        .map(xdr_bytes)
        .collect::<std::result::Result<Vec<Vec<u8>>, String>>()?;

    // One item per footprint key, read_only (XDR order) then read_write. The
    // protocol-29 host lazily decodes present entries and uses a paired TTL
    // metadata value for Soroban state. Missing entries remain explicit None.
    let mut encoded_ledger_entries: Vec<(
        Option<Vec<u8>>,
        Option<TtlLedgerEntryMeta>,
    )> = Vec::new();
    let footprint_keys = resources
        .footprint
        .read_only
        .as_slice()
        .iter()
        .chain(resources.footprint.read_write.as_slice().iter());
    for key in footprint_keys {
        let key_rc = Rc::new(key.clone());
        let got = core
            .snapshot
            .get(&key_rc)
            .map_err(|e| format!("snapshot read failed for footprint key {key:?}: {e:?}"))?;
        match got {
            Some((entry, maybe_live_until)) => {
                let encoded = xdr_bytes(entry.as_ref())?;
                let ttl = match &entry.data {
                    LedgerEntryData::ContractData(_) | LedgerEntryData::ContractCode(_) => {
                        let live_until_ledger = maybe_live_until.ok_or_else(|| {
                            format!("Soroban footprint entry {key:?} has no live-until ledger")
                        })?;
                        let entry_size_for_rent = e2e_invoke::entry_size_for_rent(
                            &budget,
                            entry.as_ref(),
                            encoded.len() as u32,
                        )
                        .map_err(|e| {
                            format!("computing rent size for footprint key {key:?} failed: {e:?}")
                        })?;
                        Some(TtlLedgerEntryMeta {
                            live_until_ledger,
                            entry_size_for_rent,
                        })
                    }
                    _ => None,
                };
                encoded_ledger_entries.push((Some(encoded), ttl));
            }
            None => encoded_ledger_entries.push((None, None)),
        }
    }

    // The hook appends one TraceStep per event; cpu/mem are cumulative budget
    // samples taken inside the hook (the same getters TraceState uses).
    let steps: Rc<RefCell<Vec<TraceStep>>> = Rc::new(RefCell::new(Vec::new()));
    let steps_in_hook = steps.clone();
    let hook: TraceHook = Rc::new(move |host, event| {
        let cpu = host.as_budget().get_cpu_insns_consumed()?;
        let mem = host.as_budget().get_mem_bytes_consumed()?;
        let display = format!("{event}");
        let (kind, name) = classify_event(&event, &display);
        let mut steps = steps_in_hook.borrow_mut();
        let seq = steps.len() as u32;
        steps.push(TraceStep {
            seq,
            kind,
            name,
            display,
            cpu,
            mem,
        });
        Ok(())
    });

    let mut diagnostic_events: Vec<DiagnosticEvent> = Vec::new();
    let replay = adapter::invoke_host_function(
        &budget,
        core.enable_diagnostics,
        encoded_host_fn,
        encoded_resources,
        &restored_rw_entry_indices,
        encoded_source_account,
        encoded_auth_entries.into_iter(),
        core.ledger_info.clone(),
        encoded_ledger_entries.into_iter(),
        core.base_prng_seed.to_vec(),
        &mut diagnostic_events,
        Some(hook),
        None,
    )
    .map_err(|e| format!("enforcing replay failed: {e:?}"))?;

    // Determinism guard: the replayed result must equal the recorded one.
    let recorded = match &core.result.invoke_result {
        Ok(val) => val,
        Err(e) => {
            // Unreachable in practice: transaction_data is None on failure,
            // which returns earlier. Kept defensive rather than panicking.
            return Err(format!(
                "pass-1 invocation failed ({e:?}); replay result cannot be cross-checked"
            ));
        }
    };
    match replay.encoded_invoke_result {
        Ok(bytes) => {
            let replayed = ScVal::from_xdr(&bytes, Limits::none())
                .map_err(|e| format!("decoding replayed result ScVal failed: {e:?}"))?;
            if &replayed != recorded {
                return Err(format!(
                    "determinism guard: replayed result {replayed:?} != recorded {recorded:?}"
                ));
            }
        }
        Err(e) => {
            return Err(format!(
                "replay invocation failed ({e:?}) but recording succeeded ({recorded:?}); \
                 likely non-replayable auth (blanked address-credential signatures)"
            ));
        }
    }

    Ok(steps.take())
}

/// Map a `TraceEvent` (+ its already-rendered `Display`) to a step kind and a
/// short name. `PushCtx`/`PopCtx` cannot be matched by variant outside
/// soroban-env-host (their `&Context` field is a private type, which is a hard
/// error at external match sites), so they are classified via their public
/// `Display` rendering: `push <frame>(<args>)` / `pop <frame> -> <result>`.
fn classify_event(event: &TraceEvent, display: &str) -> (TraceStepKind, String) {
    match event {
        TraceEvent::Begin => (TraceStepKind::Begin, "begin".to_string()),
        TraceEvent::End => (TraceStepKind::End, "end".to_string()),
        TraceEvent::EnvCall(hostfn, _) => (TraceStepKind::EnvCall, (*hostfn).to_string()),
        TraceEvent::EnvRet(hostfn, _) => (TraceStepKind::EnvRet, (*hostfn).to_string()),
        _ if display.starts_with("push ") => (
            TraceStepKind::PushCtx,
            frame_label(display, "push ", "("),
        ),
        _ => (TraceStepKind::PopCtx, frame_label(display, "pop ", " ->")),
    }
}

/// Extract the frame label from a PushCtx/PopCtx `Display` string: strip the
/// verb prefix, cut at the terminator (`(` for push args, ` ->` for pop
/// results).
fn frame_label(display: &str, prefix: &str, terminator: &str) -> String {
    let rest = display.strip_prefix(prefix).unwrap_or(display);
    match rest.find(terminator) {
        Some(idx) => rest[..idx].to_string(),
        None => rest.to_string(),
    }
}

/// Magic prefix of the encoded trace ("STRC" + version byte + NUL, same
/// framing style as the "SSIM0\0" result encoding).
pub const TRACE_MAGIC: &[u8; 6] = b"STRC0\0";

/// Encode a trace: `STRC0\0` magic, u32 step count, then per step
/// (u32 seq, u8 kind, len-prefixed name, len-prefixed display, u64 cpu,
/// u64 mem). All integers big-endian; length prefixes are u32.
pub fn encode_trace(steps: &[TraceStep]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(TRACE_MAGIC);
    let count: u32 = steps
        .len()
        .try_into()
        .map_err(|_| anyhow!("too many trace steps to encode"))?;
    out.extend_from_slice(&count.to_be_bytes());
    for step in steps {
        out.extend_from_slice(&step.seq.to_be_bytes());
        out.push(step.kind.to_u8());
        push_str(&mut out, &step.name)?;
        push_str(&mut out, &step.display)?;
        out.extend_from_slice(&step.cpu.to_be_bytes());
        out.extend_from_slice(&step.mem.to_be_bytes());
    }
    Ok(out)
}

/// Decode bytes produced by [`encode_trace`].
pub fn decode_trace(bytes: &[u8]) -> Result<Vec<TraceStep>> {
    let mut r = Reader { bytes, pos: 0 };
    let magic = r.take(TRACE_MAGIC.len())?;
    if magic != TRACE_MAGIC {
        return Err(anyhow!("bad trace magic: {magic:?}"));
    }
    let count = r.u32()?;
    let mut steps = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let seq = r.u32()?;
        let kind = TraceStepKind::from_u8(r.u8()?)?;
        let name = r.string()?;
        let display = r.string()?;
        let cpu = r.u64()?;
        let mem = r.u64()?;
        steps.push(TraceStep {
            seq,
            kind,
            name,
            display,
            cpu,
            mem,
        });
    }
    if r.pos != bytes.len() {
        return Err(anyhow!(
            "trailing bytes after trace: {} of {}",
            bytes.len() - r.pos,
            bytes.len()
        ));
    }
    Ok(steps)
}

fn push_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len: u32 = s
        .len()
        .try_into()
        .map_err(|_| anyhow!("trace string too long to encode"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn xdr_bytes<T: WriteXdr>(value: &T) -> std::result::Result<Vec<u8>, String> {
    value
        .to_xdr(DEFAULT_XDR_RW_LIMITS)
        .map_err(|e| format!("XDR encoding for replay failed: {e:?}"))
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| anyhow!("trace truncated at offset {}", self.pos))?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        Ok(String::from_utf8(self.take(len)?.to_vec())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(seq: u32, kind: TraceStepKind, name: &str, display: &str) -> TraceStep {
        TraceStep {
            seq,
            kind,
            name: name.to_string(),
            display: display.to_string(),
            cpu: 1_000 + seq as u64,
            mem: 500 + seq as u64,
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let steps = vec![
            step(0, TraceStepKind::Begin, "begin", "begin"),
            step(1, TraceStepKind::PushCtx, "VM:c288faf9:add", "push VM:c288faf9:add(I32(40), I32(2))"),
            step(2, TraceStepKind::EnvCall, "get_ledger_sequence", "call get_ledger_sequence()"),
            step(3, TraceStepKind::EnvRet, "get_ledger_sequence", "ret get_ledger_sequence -> Ok(U32(1000000))"),
            step(4, TraceStepKind::PopCtx, "VM:c288faf9:add", "pop VM:c288faf9:add -> Ok(I32(42))"),
            step(5, TraceStepKind::End, "end", "end"),
        ];
        let encoded = encode_trace(&steps).unwrap();
        assert!(encoded.starts_with(TRACE_MAGIC));
        let decoded = decode_trace(&encoded).unwrap();
        assert_eq!(decoded, steps);
    }

    #[test]
    fn decode_rejects_bad_magic_and_truncation() {
        let steps = vec![step(0, TraceStepKind::Begin, "begin", "begin")];
        let encoded = encode_trace(&steps).unwrap();

        let mut bad_magic = encoded.clone();
        bad_magic[0] = b'X';
        assert!(decode_trace(&bad_magic).is_err());

        assert!(decode_trace(&encoded[..encoded.len() - 1]).is_err());

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(decode_trace(&trailing).is_err());
    }

    #[test]
    fn frame_label_parses_push_and_pop_displays() {
        assert_eq!(
            frame_label("push VM:c288faf9:add(I32(40), I32(2))", "push ", "("),
            "VM:c288faf9:add"
        );
        assert_eq!(
            frame_label("pop VM:c288faf9:add -> Ok(I32(42))", "pop ", " ->"),
            "VM:c288faf9:add"
        );
        assert_eq!(frame_label("push SAC:abcd:transfer()", "push ", "("), "SAC:abcd:transfer");
    }
}
