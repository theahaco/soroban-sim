//! Closing a ledger the way stellar-core closes one, with any number of
//! transactions in it.
//!
//! ```text
//! submit_ledger(envelopes)
//!   build the set   each envelope validated against the last closed ledger;
//!                   one transaction per source account (core's queue holds
//!                   one; the next waits a ledger); a fee source's bids
//!                   together within its available balance (the one that
//!                   overdraws it is txINSUFFICIENT_BALANCE); within the per-ledger
//!                   limits, highest inclusion fee rate first (the rest wait)
//!   apply order     classic phase, then Soroban phase, each shuffled by
//!                   (full transaction hash XOR transaction-set hash)
//!   fees            every fee charged first, in apply order, to the fee
//!                   source (a fee bump's payer)
//!   apply           each transaction validated again against the ledger the
//!                   ones before it left, its sequence number consumed, its
//!                   pre-auth signers removed, then its operations applied
//!   refunds         Soroban refunds to the fee source once every transaction
//!                   has applied
//!   close           the ledger closes; what waited goes into the next one
//! ```
//!
//! The transaction set is the session's own: it hashes a synthetic previous
//! ledger hash, and the Soroban phase is one stage of one cluster. Core's
//! builder may split independent Soroban transactions into parallel
//! clusters, which can reorder transactions that do not conflict. No lane
//! is surge priced unless [`SubmitOptions::lane_base_fee`] says so: the
//! inclusion fee charged is the base fee per operation (a fee bump counts one
//! more).

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use soroban_env_host::storage::SnapshotSource;
use soroban_env_host::xdr::{
    AccountId, ConfigSettingEntry, ConfigSettingId, DependentTxCluster, GeneralizedTransactionSet, Hash, LedgerEntryData,
    LedgerKey, LedgerKeyConfigSetting, Limits, OperationResult, ParallelTxExecutionStage, ParallelTxsComponent, ScVal,
    TransactionEnvelope, TransactionPhase, TransactionSetV1, TxSetComponent, TxSetComponentTxsMaybeDiscountedFee,
    WriteXdr,
};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use super::applier::{remove_one_time_signer, Metrics};
use super::classic::{self, OpContext};
use super::validator::{
    available_balance, bump_sequence, declared_resources, envelope_fee_source, envelope_hash, envelope_source, transaction_hash, xlm, Rejection, TxCode,
    Valid, ValidClassicTx,
};
use super::{diagnostics_tail, Fees, Session, SubmitOptions, TxRecord, TxStatus};

/// Per-ledger maxima: the classic phase's operations (the header's
/// `maxTxSetSize`) and the Soroban phase's transactions and resources (the
/// `ConfigSetting` entries).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerLimits {
    pub max_classic_ops: u64,
    pub soroban: SorobanResources,
}

/// A Soroban transaction's declared resources, or a ledger's total of them,
/// as core counts them against the ledger limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SorobanResources {
    pub txs: u64,
    pub instructions: u64,
    pub tx_size_bytes: u64,
    pub disk_read_bytes: u64,
    pub write_bytes: u64,
    pub disk_read_entries: u64,
    pub write_entries: u64,
}

impl SorobanResources {
    fn named(&self) -> [(&'static str, u64); 7] {
        [
            ("transactions", self.txs),
            ("instructions", self.instructions),
            ("transaction size bytes", self.tx_size_bytes),
            ("disk read bytes", self.disk_read_bytes),
            ("write bytes", self.write_bytes),
            ("disk read entries", self.disk_read_entries),
            ("write entries", self.write_entries),
        ]
    }

    fn plus(&self, o: &Self) -> Self {
        Self {
            txs: self.txs.saturating_add(o.txs),
            instructions: self.instructions.saturating_add(o.instructions),
            tx_size_bytes: self.tx_size_bytes.saturating_add(o.tx_size_bytes),
            disk_read_bytes: self.disk_read_bytes.saturating_add(o.disk_read_bytes),
            write_bytes: self.write_bytes.saturating_add(o.write_bytes),
            disk_read_entries: self.disk_read_entries.saturating_add(o.disk_read_entries),
            write_entries: self.write_entries.saturating_add(o.write_entries),
        }
    }

    /// The first resource over `max`, with its total and maximum.
    fn over(&self, max: &Self) -> Option<(&'static str, u64, u64)> {
        self.named().into_iter().zip(max.named()).find(|((_, used), (_, cap))| used > cap).map(|((what, used), (_, cap))| (what, used, cap))
    }
}

impl LedgerLimits {
    pub fn unlimited() -> Self {
        let max = u64::MAX;
        Self {
            max_classic_ops: max,
            soroban: SorobanResources {
                txs: max,
                instructions: max,
                tx_size_bytes: max,
                disk_read_bytes: max,
                write_bytes: max,
                disk_read_entries: max,
                write_entries: max,
            },
        }
    }

    /// The Soroban phase's limits from the ledger; `max_tx_set_size` is the
    /// base's header value (`None`: no classic limit). The Soroban
    /// transaction count reads as unlimited where the ledger lacks it.
    pub fn load_from_snapshot(snapshot: &impl SnapshotSource, max_tx_set_size: Option<u32>) -> Result<Self> {
        let get = |id: ConfigSettingId| -> Result<Option<ConfigSettingEntry>> {
            let key = Rc::new(LedgerKey::ConfigSetting(LedgerKeyConfigSetting { config_setting_id: id }));
            Ok(match snapshot.get(&key).map_err(|e| anyhow!("reading {id:?}: {e:?}"))? {
                Some((entry, _)) => match &entry.data {
                    LedgerEntryData::ConfigSetting(cs) => Some(cs.clone()),
                    _ => None,
                },
                None => None,
            })
        };
        let mut limits = Self::unlimited();
        limits.max_classic_ops = max_tx_set_size.map_or(u64::MAX, u64::from);
        let s = &mut limits.soroban;
        if let Some(ConfigSettingEntry::ContractExecutionLanes(lanes)) = get(ConfigSettingId::ContractExecutionLanes)? {
            s.txs = lanes.ledger_max_tx_count as u64;
        }
        if let Some(ConfigSettingEntry::ContractComputeV0(c)) = get(ConfigSettingId::ContractComputeV0)? {
            s.instructions = c.ledger_max_instructions.max(0) as u64;
        }
        if let Some(ConfigSettingEntry::ContractLedgerCostV0(c)) = get(ConfigSettingId::ContractLedgerCostV0)? {
            s.disk_read_bytes = c.ledger_max_disk_read_bytes as u64;
            s.write_bytes = c.ledger_max_write_bytes as u64;
            s.disk_read_entries = c.ledger_max_disk_read_entries as u64;
            s.write_entries = c.ledger_max_write_ledger_entries as u64;
        }
        if let Some(ConfigSettingEntry::ContractBandwidthV0(b)) = get(ConfigSettingId::ContractBandwidthV0)? {
            s.tx_size_bytes = b.ledger_max_txs_size_bytes as u64;
        }
        Ok(limits)
    }
}

/// One envelope waiting for a ledger.
struct Pending<'a> {
    /// Its position in the `submit_ledger` call.
    index: usize,
    label: &'a str,
    envelope: &'a TransactionEnvelope,
    /// Why it waited for a later ledger, if it did.
    deferred: Option<String>,
}

/// A valid envelope in the set being built.
struct Candidate<'a> {
    pending: Pending<'a>,
    valid: Valid,
    /// `sha256(envelope XDR)`: core's full hash, which orders the set.
    full_hash: [u8; 32],
}

impl Candidate<'_> {
    /// The inclusion fee bid per operation, as a fraction.
    fn rate(&self) -> (i128, i128) {
        let f = self.valid.fees();
        ((f.bid - f.resource_fee) as i128, ops(&self.valid) as i128)
    }
}

/// A transaction's operations as core counts them: a fee bump counts one more.
fn ops(valid: &Valid) -> u64 {
    let ops = match valid {
        Valid::Soroban(_) => 1,
        Valid::Classic(c) => c.ops.len() as u64,
    };
    ops + valid.inner_hash().is_some() as u64
}

fn soroban_resources(valid: &Valid) -> SorobanResources {
    let Valid::Soroban(v) = valid else { return SorobanResources::default() };
    let r = &v.data.resources;
    SorobanResources {
        txs: 1,
        instructions: r.instructions as u64,
        tx_size_bytes: v.size_bytes as u64,
        disk_read_bytes: r.disk_read_bytes as u64,
        write_bytes: r.write_bytes as u64,
        disk_read_entries: declared_resources(&v.data, v.size_bytes).disk_read_entries as u64,
        write_entries: r.footprint.read_write.len() as u64,
    }
}

impl Session {
    /// Submit several envelopes for the next ledger and close ledgers until
    /// every one has applied or been refused. Returns one record per
    /// envelope, in submission order. What one ledger cannot take (a second
    /// transaction from the same source, or more than the per-ledger limits
    /// allow) waits for the next ledger, as it would in core's queue.
    /// An error leaves the session as it was before the call.
    pub fn submit_ledger(&mut self, txs: &[(&str, &TransactionEnvelope)]) -> Result<Vec<TxRecord>> {
        self.submit_ledger_with(txs, &SubmitOptions::default())
    }

    /// [`Session::submit_ledger`] with what the ledger close decides set
    /// explicitly: the lanes' base fee, and the first transaction's PRNG seed.
    pub fn submit_ledger_with(&mut self, txs: &[(&str, &TransactionEnvelope)], options: &SubmitOptions) -> Result<Vec<TxRecord>> {
        let (depth, info, logged) = (self.ledger.depth(), self.info.clone(), self.log.len());
        self.ledger.begin();
        let settled = self.settle(txs, options);
        if settled.is_ok() {
            self.ledger.commit();
        } else {
            self.ledger.rollback_to(depth);
            self.info = info;
            self.log.truncate(logged);
        }
        settled
    }

    fn settle(&mut self, txs: &[(&str, &TransactionEnvelope)], options: &SubmitOptions) -> Result<Vec<TxRecord>> {
        let mut records: Vec<Option<TxRecord>> = vec![None; txs.len()];
        let mut pending: Vec<Pending> = txs.iter().enumerate().map(|(index, (label, envelope))| Pending { index, label, envelope, deferred: None }).collect();
        while !pending.is_empty() {
            let mut waiting = vec![];
            let mut candidates = vec![];
            let mut sources = BTreeSet::new();
            let mut bids: BTreeMap<AccountId, i64> = BTreeMap::new();
            for p in pending {
                let source = envelope_source(p.envelope);
                if let Some(s) = source.as_ref().filter(|s| sources.contains(*s)) {
                    let why = format!("{s} already has a transaction in ledger {}, and core's queue holds one per account", self.info.sequence_number);
                    waiting.push(Pending { deferred: Some(why), ..p });
                    continue;
                }
                let mut verdict = self.validator_with(false, options.lane_base_fee).validate(p.envelope).map_err(|e| self.fetch_failure(e))?;
                if let Some(r) = verdict.as_ref().ok().and_then(|valid| self.ledger_refusal(valid)) {
                    verdict = Err(r);
                }
                if let Ok(valid) = &verdict {
                    let queued = bids.get(valid.fee_source()).copied().unwrap_or(0);
                    if let Some(r) = self.fee_overdraw(valid.fee_source(), queued, valid.fees().bid)? {
                        verdict = Err(r);
                    }
                }
                match verdict {
                    Ok(valid) => {
                        sources.extend(source);
                        *bids.entry(valid.fee_source().clone()).or_default() += valid.fees().bid;
                        let full_hash = Sha256::digest(p.envelope.to_xdr(Limits::none())?).into();
                        candidates.push(Candidate { pending: p, valid, full_hash });
                    }
                    Err(r) => records[p.index] = Some(self.rejected(&p, r)),
                }
            }
            let (selected, deferred) = select(candidates, &self.ledger_limits);
            waiting.extend(deferred.into_iter().map(|(c, why)| Pending { deferred: Some(why), ..c.pending }));
            waiting.sort_by_key(|p| p.index);
            // Every round settles at least one envelope: refused, or in this
            // ledger (anything deferred for the limits waits behind one that
            // got in), so the loop ends.
            if !selected.is_empty() {
                for (index, record) in self.close_with(selected, options)? {
                    records[index] = Some(record);
                }
            }
            pending = waiting;
        }
        records.into_iter().map(|r| r.ok_or_else(|| anyhow!("a submitted transaction got no record"))).collect()
    }

    /// Close one ledger with `selected`: core's apply order, fees first,
    /// apply-time validation, refunds last. Returns (submission index, record).
    fn close_with(&mut self, selected: Vec<Candidate<'_>>, options: &SubmitOptions) -> Result<Vec<(usize, TxRecord)>> {
        let seq = self.info.sequence_number;
        let set_hash = self.tx_set_hash(&selected)?;
        let (mut order, soroban): (Vec<_>, Vec<_>) = selected.into_iter().partition(|c| !c.valid.is_soroban());
        let by_set_hash = |a: &Candidate, b: &Candidate| xored(&a.full_hash, &set_hash).cmp(&xored(&b.full_hash, &set_hash));
        order.sort_by(by_set_hash);
        let mut soroban = soroban;
        soroban.sort_by(by_set_hash);
        order.extend(soroban);

        // Every fee first, from the balance before any transaction applies.
        let mut charged = Vec::with_capacity(order.len());
        for c in &order {
            let fee = c.valid.fees().charged_up_front();
            let source = c.valid.fee_source();
            let mut payer = self.ledger.account(source)?.ok_or_else(|| anyhow!("fee source {source} vanished"))?;
            let fee = fee.min(payer.balance);
            payer.balance -= fee;
            self.ledger.put_account(payer, seq)?;
            charged.push(fee);
        }

        let mut out = vec![];
        let mut refunds = vec![];
        for (i, (c, fee)) in order.into_iter().zip(charged).enumerate() {
            let mut record = self.blank_record(c.pending.label, c.pending.envelope);
            record.application_order = Some(i as u32 + 1);
            record.deferred = c.pending.deferred.clone();
            record.fees = *c.valid.fees();
            record.fee_charged = fee;
            // A given seed is for the first transaction (a replayed one).
            let seed = match options.prng_seed {
                Some(seed) if i == 0 => seed,
                _ => core_prng_seed(&set_hash, i as u64),
            };
            let refund = self.apply_one(&c, &mut record, seed, options)?;
            if refund > 0 {
                refunds.push((c.valid.fee_source().clone(), refund, out.len()));
            }
            out.push((c.pending.index, record));
        }
        // Soroban refunds once the whole set has applied, to the fee source;
        // nothing comes back to an account that is gone (core's
        // `refundSorobanFee`).
        for (payer, refund, at) in refunds {
            let Some(mut account) = self.ledger.account(&payer)? else { continue };
            let Some(balance) = account.balance.checked_add(refund) else { continue };
            account.balance = balance;
            self.ledger.put_account(account, seq)?;
            let record = &mut out[at].1;
            record.refund = refund;
            record.fee_charged -= refund;
        }
        for (_, r) in &out {
            self.log.push(r.clone());
        }
        self.close_ledger();
        Ok(out)
    }

    /// Validate `c` again against the ledger as it is now, consume its
    /// sequence number, then apply it. Returns the refund to credit later.
    fn apply_one(&mut self, c: &Candidate<'_>, record: &mut TxRecord, seed: [u8; 32], options: &SubmitOptions) -> Result<i64> {
        let seq = self.info.sequence_number;
        // A fee bump first spends its fee source's pre-auth signer, then
        // applies the inner transaction (core's `FeeBumpTransactionFrame::apply`).
        if c.valid.inner_hash().is_some() {
            remove_one_time_signer(&self.ledger, c.valid.fee_source(), c.valid.hash(), seq)?;
        }
        let verdict = self.validator_with(true, options.lane_base_fee).validate_at_apply(c.pending.envelope).map_err(|e| self.fetch_failure(e))?;
        // Core consumes the sequence number once the sequence check passed,
        // and drops the transaction's pre-auth signers whatever happens next.
        let seq_passed = match &verdict {
            Ok(_) => true,
            Err(r) => !matches!(
                r.code,
                TxCode::Malformed
                    | TxCode::MissingOperation
                    | TxCode::NotSupported
                    | TxCode::TooEarly
                    | TxCode::TooLate
                    | TxCode::InsufficientFee
                    | TxCode::NoAccount
                    | TxCode::FrozenKeyAccessed
                    | TxCode::BadSeq
                    | TxCode::SorobanInvalid
            ),
        };
        if seq_passed {
            let source = c.valid.source();
            let mut account = self.ledger.account(source)?.ok_or_else(|| anyhow!("source account {source} vanished"))?;
            bump_sequence(&mut account, c.valid.seq_num(), seq, self.info.timestamp);
            self.ledger.put_account(account, seq)?;
        }
        let tx_hash = c.valid.inner_hash().unwrap_or(c.valid.hash());
        for id in std::iter::once(c.valid.source().clone()).chain(c.valid.op_sources()) {
            remove_one_time_signer(&self.ledger, &id, tx_hash, seq)?;
        }
        if let Err(r) = verdict {
            record.failed_code = Some(r.code);
            record.failed_fee_bump_inner = r.fee_bump_inner;
            record.op_results = r.op_results;
            record.status = TxStatus::Failed { code: None, reason: format!("{} at apply: {}", super::rejection_code(r.code, r.fee_bump_inner), r.detail) };
            // No operation ran: the whole refundable fee comes back.
            return Ok(c.valid.fees().refundable);
        }
        record.signers = c.valid.signers().to_vec();
        record.replayable = c.valid.replayable().to_vec();
        match &c.valid {
            Valid::Soroban(v) => {
                let applied = self
                    .applier
                    .apply(&self.ledger, &self.info, &self.network_config, &self.limits, v, self.options.enable_diagnostics, seed)
                    .map_err(|e| self.fetch_failure(e))?;
                record.rent_fee = applied.rent_fee;
                record.events_fee = applied.events_fee;
                record.cpu_insns = applied.cpu_insns;
                record.declared_insns = v.data.resources.instructions;
                record.mem_bytes = applied.mem_bytes;
                record.write_bytes = applied.write_bytes;
                record.declared_write_bytes = v.data.resources.write_bytes;
                record.entries_written = applied.entries_written;
                record.restored = applied.restored;
                record.events = applied.events;
                record.diagnostics = applied.diagnostics;
                record.metrics = applied.metrics;
                record.success_hash = applied.success_hash;
                record.status = match applied.result {
                    Ok(value) => TxStatus::Applied { value },
                    Err(f) => TxStatus::Failed { code: Some(f.code), reason: format!("{}{}", f.reason, diagnostics_tail(&record.diagnostics)) },
                };
                Ok(applied.refund)
            }
            Valid::Classic(v) => {
                self.apply_classic(v, record)?;
                Ok(0)
            }
        }
    }

    /// Apply a classic transaction's operations in order. Every operation
    /// runs and gets its result, but the transaction's writes stay only if
    /// all of them succeeded.
    fn apply_classic(&mut self, tx: &ValidClassicTx, record: &mut TxRecord) -> Result<()> {
        let ctx = OpContext { ledger_seq: self.info.sequence_number, base_reserve: self.info.base_reserve };
        let mut results = Vec::with_capacity(tx.ops.len());
        let mut changes = Vec::with_capacity(tx.ops.len());
        let mut failures = vec![];
        self.ledger.begin();
        for (i, op) in tx.ops.iter().enumerate() {
            self.ledger.begin();
            let result = if self.ledger.account(&op.source)?.is_none() {
                OperationResult::OpNoAccount
            } else if let Some(r) = classic::check_valid(&op.body, &op.source) {
                r
            } else {
                classic::apply(&self.ledger, &ctx, &op.source, &op.body).map_err(|e| self.fetch_failure(e))?
            };
            if classic::is_success(&result) {
                changes.push(self.ledger.open_changes()?);
                self.ledger.commit();
            } else {
                self.ledger.rollback();
                failures.push(format!("operation {} ({}): {}", i + 1, op.body.name(), classic::result_name(&result)));
            }
            results.push(result);
        }
        if failures.is_empty() {
            record.entries_written = self.ledger.open_writes() as u32;
            self.ledger.commit();
            record.status = TxStatus::Applied { value: ScVal::Void };
            record.op_changes = changes;
        } else {
            self.ledger.rollback();
            record.status = TxStatus::Failed { code: None, reason: failures.join("; ") };
        }
        record.op_results = results;
        Ok(())
    }

    /// Refused with txSOROBAN_INVALID when `valid` alone is over the
    /// per-ledger limits, so no ledger can hold it.
    pub fn ledger_refusal(&self, valid: &Valid) -> Option<Rejection> {
        let limits = &self.ledger_limits;
        let detail = if valid.is_soroban() {
            let (what, used, cap) = soroban_resources(valid).over(&limits.soroban)?;
            format!("its {what} ({used}) exceed the per-ledger maximum {cap}")
        } else {
            let ops = ops(valid);
            if ops <= limits.max_classic_ops {
                return None;
            }
            format!("its {ops} operations exceed the per-ledger maximum {}", limits.max_classic_ops)
        };
        Some(Rejection { code: TxCode::SorobanInvalid, detail, fee_bump_inner: false, op_results: vec![] })
    }

    /// core's queue `canAfford`: refused with txINSUFFICIENT_BALANCE when
    /// `bid` on top of the `queued` bids `fee_source` already pays for in
    /// this ledger is more than its balance above the reserve.
    pub fn fee_overdraw(&self, fee_source: &AccountId, queued: i64, bid: i64) -> Result<Option<Rejection>> {
        let account = self.ledger.account(fee_source)?.ok_or_else(|| anyhow!("fee source {fee_source} vanished"))?;
        let available = available_balance(&account, self.info.base_reserve);
        let total = queued.saturating_add(bid);
        if total <= available {
            return Ok(None);
        }
        let detail = format!(
            "fee source {fee_source} would pay {} XLM in fee bids this ledger ({} XLM already queued + {} XLM), available {} XLM above the reserve",
            xlm(total),
            xlm(queued),
            xlm(bid),
            xlm(available)
        );
        Ok(Some(Rejection { code: TxCode::InsufficientBalance, detail, fee_bump_inner: false, op_results: vec![] }))
    }

    /// Record `envelope` as refused before it reached a ledger.
    pub fn refuse(&mut self, label: &str, envelope: &TransactionEnvelope, r: Rejection) -> TxRecord {
        self.rejected(&Pending { index: 0, label, envelope, deferred: None }, r)
    }

    /// The record of an envelope refused before it reached a ledger.
    fn rejected(&mut self, p: &Pending<'_>, r: Rejection) -> TxRecord {
        let mut record = self.blank_record(p.label, p.envelope);
        record.deferred = p.deferred.clone();
        record.status = TxStatus::Rejected { code: r.code, detail: r.detail, fee_bump_inner: r.fee_bump_inner };
        record.op_results = r.op_results;
        self.log.push(record.clone());
        record
    }

    fn blank_record(&self, label: &str, envelope: &TransactionEnvelope) -> TxRecord {
        TxRecord {
            label: label.to_string(),
            ledger: self.info.sequence_number,
            close_time: self.info.timestamp,
            hash: envelope_hash(envelope, self.info.network_id).unwrap_or_default(),
            inner_hash: match envelope {
                TransactionEnvelope::TxFeeBump(e) => {
                    let soroban_env_host::xdr::FeeBumpTransactionInnerTx::Tx(inner) = &e.tx.inner_tx;
                    transaction_hash(&inner.tx, self.info.network_id).ok()
                }
                _ => None,
            },
            source: envelope_source(envelope),
            fee_source: envelope_fee_source(envelope),
            status: TxStatus::Rejected { code: TxCode::Malformed, detail: String::new(), fee_bump_inner: false },
            fees: Fees::default(),
            fee_charged: 0,
            refund: 0,
            rent_fee: 0,
            events_fee: 0,
            cpu_insns: 0,
            declared_insns: 0,
            mem_bytes: 0,
            write_bytes: 0,
            declared_write_bytes: 0,
            entries_written: 0,
            restored: 0,
            events: vec![],
            diagnostics: vec![],
            metrics: Metrics::default(),
            success_hash: None,
            signers: vec![],
            replayable: vec![],
            op_results: vec![],
            op_changes: vec![],
            failed_code: None,
            failed_fee_bump_inner: false,
            application_order: None,
            deferred: None,
        }
    }

    /// The contents hash of the session's transaction set for this ledger:
    /// a protocol-23 generalized set with a classic phase and a one-cluster
    /// parallel Soroban phase, each sorted by full hash, over a synthetic
    /// previous ledger hash.
    fn tx_set_hash(&self, selected: &[Candidate<'_>]) -> Result<[u8; 32]> {
        let mut classic: Vec<&Candidate> = selected.iter().filter(|c| !c.valid.is_soroban()).collect();
        let mut soroban: Vec<&Candidate> = selected.iter().filter(|c| c.valid.is_soroban()).collect();
        classic.sort_by_key(|c| c.full_hash);
        soroban.sort_by_key(|c| c.full_hash);
        let envelopes = |cs: &[&Candidate]| -> Result<_> { Ok(cs.iter().map(|c| c.pending.envelope.clone()).collect::<Vec<_>>().try_into()?) };
        let base_fee = Some(self.fork.base_fee as i64);
        let classic_phase = if classic.is_empty() {
            vec![]
        } else {
            vec![TxSetComponent::TxsetCompTxsMaybeDiscountedFee(TxSetComponentTxsMaybeDiscountedFee { base_fee, txs: envelopes(&classic)? })]
        };
        let stages = if soroban.is_empty() { vec![] } else { vec![ParallelTxExecutionStage(vec![DependentTxCluster(envelopes(&soroban)?)].try_into()?)] };
        let set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash(self.previous_ledger_hash()),
            phases: vec![
                TransactionPhase::V0(classic_phase.try_into()?),
                TransactionPhase::V1(ParallelTxsComponent { base_fee, execution_stages: stages.try_into()? }),
            ]
            .try_into()?,
        });
        Ok(Sha256::digest(set.to_xdr(Limits::none())?).into())
    }

    /// The session does not hash its ledgers: a stand-in for the hash of the
    /// ledger before the one being closed.
    fn previous_ledger_hash(&self) -> [u8; 32] {
        let prev = self.info.sequence_number - 1;
        Sha256::digest([b"soroban-sim ledger".as_slice(), &self.info.network_id, &prev.to_be_bytes()].concat()).into()
    }
}

/// Candidates set aside, each with the reason.
type SetAside<'a> = Vec<(Candidate<'a>, String)>;

/// Split the valid candidates (each fits an empty ledger) into this ledger's
/// set and those that wait for the next ledger. Highest inclusion fee rate per
/// operation first, then submission order (core breaks ties at random).
fn select<'a>(mut candidates: Vec<Candidate<'a>>, limits: &LedgerLimits) -> (Vec<Candidate<'a>>, SetAside<'a>) {
    candidates.sort_by(|a, b| {
        let ((an, ad), (bn, bd)) = (a.rate(), b.rate());
        (bn * ad).cmp(&(an * bd)).then(a.pending.index.cmp(&b.pending.index))
    });
    let (mut selected, mut deferred) = (vec![], vec![]);
    let mut classic_ops = 0u64;
    let mut soroban = SorobanResources::default();
    for c in candidates {
        if c.valid.is_soroban() {
            let r = soroban_resources(&c.valid);
            if let Some((what, used, cap)) = soroban.plus(&r).over(&limits.soroban) {
                deferred.push((c, format!("the Soroban phase was full ({what}: {used} > {cap})")));
            } else {
                soroban = soroban.plus(&r);
                selected.push(c);
            }
        } else {
            let ops = ops(&c.valid);
            if classic_ops + ops > limits.max_classic_ops {
                deferred.push((c, format!("the classic phase was full ({} operations > {})", classic_ops + ops, limits.max_classic_ops)));
            } else {
                classic_ops += ops;
                selected.push(c);
            }
        }
    }
    (selected, deferred)
}

fn xored(h: &[u8; 32], x: &[u8; 32]) -> [u8; 32] {
    std::array::from_fn(|i| h[i] ^ x[i])
}

/// stellar-core's per-transaction PRNG seed: `sha256(txSetContentsHash ||
/// applyIndex as XDR u64)`, the index counting every transaction of the set
/// in apply order.
pub fn core_prng_seed(set_hash: &[u8; 32], apply_index: u64) -> [u8; 32] {
    Sha256::digest([set_hash.as_slice(), &apply_index.to_be_bytes()].concat()).into()
}
