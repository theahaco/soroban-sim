//! Network-session engine: carried state, the transaction layer, the applier,
//! the clock and the report data. The negative controls are the ones the
//! netsim prototype ran against mainnet, rebuilt over a static fork.

mod common;

use common::*;
use soroban_sim_core::host::xdr::{
    Duration, Int128Parts, LedgerBounds, LedgerKey, LedgerKeyContractCode, Preconditions,
    PreconditionsV2, ScAddress, ScVal, SorobanCredentials, TimeBounds, TimePoint, TransactionExt,
};
use soroban_sim_core::session::*;
use std::collections::BTreeMap;

fn add_contract(s: &mut Session, key: &Ed25519Key) -> ScAddress {
    deploy(s, &alice(), key, "add_i32", "add", vec![])
}

fn i128(n: i64) -> ScVal {
    ScVal::I128(Int128Parts { hi: if n < 0 { -1 } else { 0 }, lo: n as u64 })
}

#[test]
fn state_carries_across_upload_deploy_invoke() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let start_balance = balance(&s, &alice());
    let start_seq = seq(&s, &alice());
    let start_ledger = s.ledger_info().sequence_number;

    let add = add_contract(&mut s, &key);
    let sum = apply(&mut s, "add", &alice(), &[key], invoke(&add, "add", vec![ScVal::I32(40), ScVal::I32(2)]));
    assert_eq!(sum, ScVal::I32(42));

    // Three transactions, three ledgers, three sequence numbers.
    assert_eq!(s.log().len(), 3);
    assert_eq!(s.ledger_info().sequence_number, start_ledger + 3);
    assert_eq!(seq(&s, &alice()), start_seq + 3);
    let charged: i64 = s.log().iter().map(|t| t.fee_charged).sum();
    assert_eq!(balance(&s, &alice()), start_balance - charged);
    for t in s.log() {
        assert!(t.status.is_applied());
        assert_eq!(t.fee_charged + t.refund, t.fees.charged_up_front(), "{}", t.label);
        assert!(t.refund > 0, "{}: an upload or call refunds unused rent", t.label);
        assert!(t.cpu_insns <= t.declared_insns as u64);
    }

    // The code is in the module cache once a transaction used it.
    assert!(s.applier().cached_modules().contains(&soroban_sim_core::host::xdr::Hash(sha256(&wasm("add_i32")))));

    // The final diff: alice's account, the code, the instance. One override.
    let diff = s.ledger().diff();
    let kinds: Vec<(&str, Change)> = diff.iter().map(|d| (d.key.name(), d.change())).collect();
    assert!(kinds.contains(&("Account", Change::Updated)));
    assert!(kinds.contains(&("ContractCode", Change::Created)));
    assert!(kinds.contains(&("ContractData", Change::Created)));
    assert!(diff.iter().find(|d| d.key == account_key(&alice())).unwrap().overridden);
    let overrides = s.ledger().overrides();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0].kind, OverrideKind::SignerSubstitution);
    assert!(overrides[0].detail.contains("master key weight 1 -> 0"));
}

#[test]
fn envelope_signed_by_a_non_signer_is_bad_auth() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let add = add_contract(&mut s, &key);
    let ledger = s.ledger_info().sequence_number;
    let (bal, sq) = (balance(&s, &alice()), seq(&s, &alice()));
    let p = prepare(&s, &alice(), &[Ed25519Key::throwaway("not-a-signer")], invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(2)]), &no_auth());
    let r = s.submit("non-signer", &p.envelope).unwrap();
    expect_rejected(&r, TxCode::BadAuth);
    // Rejected envelopes pay nothing, consume no sequence number, close no ledger.
    assert_eq!((balance(&s, &alice()), seq(&s, &alice())), (bal, sq));
    assert_eq!(s.ledger_info().sequence_number, ledger);
}

#[test]
fn the_substituted_real_key_no_longer_signs() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let add = add_contract(&mut s, &key);
    // The master key's weight moved to the throwaway: the real key is weight 0.
    let p = prepare(&s, &alice(), &[real_key("alice")], invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(2)]), &no_auth());
    expect_rejected(&s.submit("real key", &p.envelope).unwrap(), TxCode::BadAuth);
}

#[test]
fn replayed_envelope_is_bad_seq() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let add = add_contract(&mut s, &key);
    let p = prepare(&s, &alice(), &[key], invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(2)]), &no_auth());
    assert!(s.submit("first", &p.envelope).unwrap().status.is_applied());
    expect_rejected(&s.submit("replay", &p.envelope).unwrap(), TxCode::BadSeq);
}

#[test]
fn auth_entry_signed_by_the_wrong_key_fails_in_the_host() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = Ed25519Key::throwaway("carol");
    s.fund(&carol.account_id(), 100 * XLM, "carol").unwrap();
    apply(&mut s, "deploy native SAC", &carol.account_id(), std::slice::from_ref(&carol), native_sac());
    let sac = sac_id();
    // carol pays; alice must authorize the transfer out of her account.
    let transfer = invoke(&sac, "transfer", vec![ScVal::Address(ScAddress::Account(alice())), ScVal::Address(ScAddress::Account(carol.account_id())), i128(10 * XLM)]);
    let right = BTreeMap::from([(ScAddress::Account(alice()), AuthSigner::classic(vec![key.clone()]))]);
    let wrong = BTreeMap::from([(ScAddress::Account(alice()), AuthSigner::classic(vec![Ed25519Key::throwaway("mallory")]))]);

    // Enforcing simulation already refuses the wrong signature...
    let err = s
        .prepare(&PrepareRequest {
            source: carol.account_id(),
            host_function: transfer.clone(),
            auth_signers: &wrong,
            envelope_signers: std::slice::from_ref(&carol),
            auth_expiration_ledgers: None,
            memo: soroban_sim_core::host::xdr::Memo::None,
            preconditions: Preconditions::None,
        })
        .unwrap_err();
    assert!(format!("{err:#}").contains("Auth"), "{err:#}");

    // ...and so does the applier, for an envelope whose auth entry was
    // swapped for the wrong key's signature after preparation.
    let p = prepare(&s, &carol.account_id(), std::slice::from_ref(&carol), transfer.clone(), &right);
    let mut forged = p.auth.clone();
    let SorobanCredentials::Address(c) = &forged[0].credentials else { panic!("address credentials") };
    let expiration = c.signature_expiration_ledger;
    sign_auth_entry(&mut forged[0], &wrong[&ScAddress::Account(alice())], s.network_id(), expiration).unwrap();
    let env = resign(&p.envelope, std::slice::from_ref(&carol), |tx| {
        let mut ops = tx.operations.to_vec();
        let soroban_sim_core::host::xdr::OperationBody::InvokeHostFunction(op) = &mut ops[0].body else { panic!() };
        op.auth = forged.try_into().unwrap();
        tx.operations = ops.try_into().unwrap();
    });
    let carol_before = balance(&s, &carol.account_id());
    let r = s.submit("forged auth", &env).unwrap();
    expect_failed(&r, "Auth");
    // A failed transaction still pays its non-refundable fee.
    assert!(r.fee_charged > 0);
    assert_eq!(balance(&s, &carol.account_id()), carol_before - r.fee_charged);

    // The correctly signed one moves the money; carol (the payer) is in the
    // footprint, so the refund lands on her post-transfer balance.
    let alice_before = balance(&s, &alice());
    let carol_before = balance(&s, &carol.account_id());
    let p = prepare(&s, &carol.account_id(), std::slice::from_ref(&carol), transfer, &right);
    let r = s.submit("transfer", &p.envelope).unwrap();
    assert!(r.status.is_applied(), "{}", r.status.summary());
    assert_eq!(balance(&s, &alice()), alice_before - 10 * XLM);
    assert_eq!(balance(&s, &carol.account_id()), carol_before + 10 * XLM - r.fee_charged);
}

#[test]
fn a_fresh_fork_does_not_have_the_session_contract() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let add = add_contract(&mut s, &key);
    let fresh = session();
    let err = fresh.view(&invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(2)])).unwrap().unwrap_err();
    assert!(err.contains("Storage") && err.contains("MissingValue"), "{err}");
    // The session itself answers, and the second contract-data read was
    // answered locally because the session created the contract.
    assert_eq!(s.view(&invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(2)])).unwrap().unwrap(), ScVal::I32(3));
}

#[test]
fn an_expired_auth_signature_fails_at_apply_and_still_pays() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = Ed25519Key::throwaway("carol");
    s.fund(&carol.account_id(), 100 * XLM, "carol").unwrap();
    apply(&mut s, "deploy native SAC", &carol.account_id(), std::slice::from_ref(&carol), native_sac());
    let transfer = invoke(&sac_id(), "transfer", vec![ScVal::Address(ScAddress::Account(alice())), ScVal::Address(ScAddress::Account(carol.account_id())), i128(XLM)]);
    let signers = BTreeMap::from([(ScAddress::Account(alice()), AuthSigner::classic(vec![key]))]);
    let p = s
        .prepare(&PrepareRequest {
            source: carol.account_id(),
            host_function: transfer,
            auth_signers: &signers,
            envelope_signers: std::slice::from_ref(&carol),
            auth_expiration_ledgers: Some(5),
            memo: soroban_sim_core::host::xdr::Memo::None,
            preconditions: Preconditions::None,
        })
        .unwrap();
    s.advance(10, None);
    let sq = seq(&s, &carol.account_id());
    let ledger = s.ledger_info().sequence_number;
    let r = s.submit("expired signature", &p.envelope).unwrap();
    expect_failed(&r, "Auth");
    assert!(r.fee_charged > 0 && r.refund == r.fees.refundable);
    assert_eq!(seq(&s, &carol.account_id()), sq + 1, "a failed transaction consumes its sequence number");
    assert_eq!(s.ledger_info().sequence_number, ledger + 1, "and its ledger closes");
}

#[test]
fn archived_entries_must_be_restored() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let store = deploy(&mut s, &alice(), &key, "storage", "storage", vec![]);
    apply(&mut s, "put", &alice(), std::slice::from_ref(&key), invoke(&store, "put_persist", vec![ScVal::U32(1), ScVal::U32(7)]));
    let get = invoke(&store, "get_persist", vec![ScVal::U32(1)]);
    let stale = prepare(&s, &alice(), std::slice::from_ref(&key), get.clone(), &no_auth());

    // Past every TTL: the instance, the code and the entry are archived.
    s.advance(s.ledger_info().min_persistent_entry_ttl + 10, None);
    let code_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: soroban_sim_core::host::xdr::Hash(sha256(&wasm("storage"))) });
    let code = s.ledger().get(&code_key).unwrap();
    assert_eq!(EntryState::of(code.as_ref(), s.ledger_info().sequence_number), EntryState::Archived);

    // An envelope prepared before archival has no restore marks: it fails.
    let r = s.submit("stale get", &stale.envelope).unwrap();
    expect_failed(&r, "ENTRY_ARCHIVED");
    // Prepared now, simulation marks the archived entries for restore.
    let fresh = prepare(&s, &alice(), std::slice::from_ref(&key), get, &no_auth());
    let r = s.submit("restoring get", &fresh.envelope).unwrap();
    assert_eq!(r.status.summary(), "7");
    assert!(r.restored >= 3, "instance, code and entry restored, got {}", r.restored);
    let code = s.ledger().get(&code_key).unwrap();
    assert_eq!(EntryState::of(code.as_ref(), s.ledger_info().sequence_number), EntryState::Live);
}

#[test]
fn expired_temporary_entries_are_absent() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let store = deploy(&mut s, &alice(), &key, "storage", "storage", vec![]);
    apply(&mut s, "put temp", &alice(), std::slice::from_ref(&key), invoke(&store, "put_temp", vec![ScVal::U32(1), ScVal::U32(9)]));
    assert_eq!(s.view(&invoke(&store, "get_temp", vec![ScVal::U32(1)])).unwrap().unwrap(), ScVal::U32(9));
    s.advance(s.ledger_info().min_temp_entry_ttl + 10, None);
    // The instance is still live (persistent TTL); the temporary entry is gone.
    let err = s.view(&invoke(&store, "get_temp", vec![ScVal::U32(1)])).unwrap().unwrap_err();
    assert!(err.contains("WasmVm") || err.contains("UnreachableCodeReached") || err.contains("Context"), "{err}");
}

#[test]
fn multisig_thresholds_follow_signature_checker() {
    let mut s = session();
    let master = Ed25519Key::throwaway("bob");
    let cosigner = Ed25519Key::throwaway("bob-cosigner");
    s.substitute_master_key(&bob(), &master, "bob master").unwrap();
    s.substitute_signer(&bob(), real_key("bob-cosigner").public_key(), &cosigner, "bob cosigner").unwrap();
    let alice_key = rehearse_alice(&s);
    let add = add_contract(&mut s, &alice_key);
    let call = invoke(&add, "add", vec![ScVal::I32(2), ScVal::I32(2)]);

    // Low threshold 1 is met by one signer, InvokeHostFunction's medium 2 is not.
    let p = prepare(&s, &bob(), std::slice::from_ref(&master), call.clone(), &no_auth());
    expect_rejected(&s.submit("one of two", &p.envelope).unwrap(), TxCode::OpBadAuth);
    // A signature nobody needed is BAD_AUTH_EXTRA.
    let p = prepare(&s, &bob(), &[master.clone(), cosigner.clone(), Ed25519Key::throwaway("bystander")], call.clone(), &no_auth());
    expect_rejected(&s.submit("extra signature", &p.envelope).unwrap(), TxCode::BadAuthExtra);
    // Both signers reach medium.
    let p = prepare(&s, &bob(), &[master.clone(), cosigner.clone()], call, &no_auth());
    let r = s.submit("two of two", &p.envelope).unwrap();
    assert!(r.status.is_applied(), "{}", r.status.summary());
    assert_eq!(r.signers.iter().map(|m| m.weight).sum::<u32>(), 2);
    assert!(r.replayable.is_empty());
}

#[test]
fn signing_with_a_base_signer_is_flagged_replayable() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let add = add_contract(&mut s, &key);
    // bob is NOT substituted: his real keys sign, so this envelope would be
    // valid on the base network as well.
    let p = prepare(&s, &bob(), &[real_key("bob"), real_key("bob-cosigner")], invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(1)]), &no_auth());
    let r = s.submit("real keys", &p.envelope).unwrap();
    assert!(r.status.is_applied());
    assert_eq!(r.replayable.len(), 2, "{:?}", r.replayable);
}

#[test]
fn auth_entry_signed_by_a_base_signer_is_flagged_replayable() {
    let mut s = session();
    let carol = Ed25519Key::throwaway("carol");
    s.fund(&carol.account_id(), 100 * XLM, "carol").unwrap();
    apply(&mut s, "deploy native SAC", &carol.account_id(), std::slice::from_ref(&carol), native_sac());
    // carol (a session account) signs the envelope; bob's real, unsubstituted
    // keys sign the auth entry, which is valid on the base network too.
    let transfer = invoke(&sac_id(), "transfer", vec![ScVal::Address(ScAddress::Account(bob())), ScVal::Address(ScAddress::Account(carol.account_id())), i128(XLM)]);
    let auth = BTreeMap::from([(ScAddress::Account(bob()), AuthSigner::classic(vec![real_key("bob"), real_key("bob-cosigner")]))]);
    let p = prepare(&s, &carol.account_id(), std::slice::from_ref(&carol), transfer, &auth);
    let r = s.submit("real auth keys", &p.envelope).unwrap();
    assert!(r.status.is_applied(), "{}", r.status.summary());
    assert_eq!(r.replayable.len(), 2, "{:?}", r.replayable);
    assert!(r.replayable.iter().all(|x| x.contains("auth entry")), "{:?}", r.replayable);
}

#[test]
fn min_seq_num_leaves_the_account_at_the_transaction_sequence() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let add = add_contract(&mut s, &key);
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(1)]), &no_auth());
    let at = seq(&s, &alice());
    let jump = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        tx.seq_num.0 = at + 5;
        tx.cond = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: Some(soroban_sim_core::host::xdr::SequenceNumber(at)),
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: Default::default(),
        })
    });
    assert!(s.submit("jump", &jump).unwrap().status.is_applied());
    assert_eq!(seq(&s, &alice()), at + 5);
}

#[test]
fn removing_an_unfetched_base_entry_is_in_the_diff() {
    let s = session();
    let key = account_key(&bob());
    s.ledger().remove_entry(&key, "drop bob", s.ledger_info().sequence_number).unwrap();
    let diff = s.ledger().diff();
    let d = diff.iter().find(|d| d.key == key).expect("bob's removal is in the diff");
    assert_eq!(d.change(), Change::Deleted);
    assert!(s.ledger().changed_keys().contains(&key));
}

#[test]
fn balance_must_cover_the_fee_bid_above_the_reserve() {
    let mut s = session();
    let poor = Ed25519Key::throwaway("poor");
    // 1 XLM is the bare reserve of a new account: nothing left for fees.
    s.fund(&poor.account_id(), XLM, "poor").unwrap();
    let p = prepare(&s, &poor.account_id(), std::slice::from_ref(&poor), native_sac(), &no_auth());
    let r = s.submit("poor", &p.envelope).unwrap();
    expect_rejected(&r, TxCode::InsufficientBalance);
    s.fund(&poor.account_id(), 10 * XLM, "poor top-up").unwrap();
    assert!(s.submit("poor, topped up", &p.envelope).unwrap().status.is_applied());
}

#[test]
fn fee_and_resource_checks() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let add = add_contract(&mut s, &key);
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(1)]), &no_auth());

    let low_bid = resign(&p.envelope, std::slice::from_ref(&key), |tx| tx.fee -= 1);
    expect_rejected(&s.submit("bid below resource fee + base fee", &low_bid).unwrap(), TxCode::InsufficientFee);

    let no_resource_fee = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        let TransactionExt::V1(d) = &mut tx.ext else { panic!() };
        d.resource_fee = 0;
    });
    expect_rejected(&s.submit("resource fee 0", &no_resource_fee).unwrap(), TxCode::SorobanInvalid);

    let over_limit = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        let TransactionExt::V1(d) = &mut tx.ext else { panic!() };
        d.resources.instructions = u32::MAX;
    });
    let r = s.submit("instructions above the network maximum", &over_limit).unwrap();
    expect_rejected(&r, TxCode::SorobanInvalid);

    // Declaring fewer write bytes than the call writes is caught after the
    // host returns: the operation fails, the fee is paid.
    let store = deploy(&mut s, &alice(), &key, "storage", "storage", vec![]);
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), invoke(&store, "put_persist", vec![ScVal::U32(3), ScVal::U32(4)]), &no_auth());
    let short = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        let TransactionExt::V1(d) = &mut tx.ext else { panic!() };
        d.resources.write_bytes = 1;
    });
    let r = s.submit("write bytes short", &short).unwrap();
    expect_failed(&r, "RESOURCE_LIMIT_EXCEEDED");
}

#[test]
fn time_and_ledger_bounds() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let add = add_contract(&mut s, &key);
    let p = prepare(&s, &alice(), std::slice::from_ref(&key), invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(1)]), &no_auth());
    let now = s.ledger_info().timestamp;
    let early = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        tx.cond = Preconditions::Time(TimeBounds { min_time: TimePoint(now + 3600), max_time: TimePoint(0) })
    });
    expect_rejected(&s.submit("too early", &early).unwrap(), TxCode::TooEarly);
    let late = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        tx.cond = Preconditions::Time(TimeBounds { min_time: TimePoint(0), max_time: TimePoint(now - 1) })
    });
    expect_rejected(&s.submit("too late", &late).unwrap(), TxCode::TooLate);
    let seq_now = s.ledger_info().sequence_number;
    let ledger_bound = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        tx.cond = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: Some(LedgerBounds { min_ledger: seq_now + 100, max_ledger: 0 }),
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: Default::default(),
        })
    });
    expect_rejected(&s.submit("ledger too early", &ledger_bound).unwrap(), TxCode::TooEarly);
    // Time travel: close ledgers until both bounds open.
    s.warp_to_time(now + 3600).unwrap();
    assert!(s.ledger_info().sequence_number >= seq_now + 100);
    assert!(s.submit("in bounds", &early).unwrap().status.is_applied());
}

#[test]
fn clock_moves_sequence_and_time_together() {
    let mut s = session();
    let (seq0, t0) = (s.ledger_info().sequence_number, s.ledger_info().timestamp);
    assert_eq!((seq0, t0), (FORK_LEDGER + 1, FORK_TIME + 5));
    s.advance(539, None);
    assert_eq!((s.ledger_info().sequence_number, s.ledger_info().timestamp), (seq0 + 539, t0 + 539 * 5));
    s.advance(1, Some(3600));
    assert_eq!(s.ledger_info().timestamp, t0 + 539 * 5 + 3600);
    s.warp_to_time(t0 + 539 * 5 + 3600 + 11).unwrap();
    assert_eq!(s.ledger_info().sequence_number, seq0 + 540 + 3);
    assert!(s.warp_to_ledger(seq0).is_err());
}

#[test]
fn drift_lists_entries_modified_after_the_fork_point() {
    let late = Ed25519Key::throwaway("late-writer");
    let mut entry = new_account(late.account_id(), 50 * XLM, FORK_LEDGER - 10);
    entry.balance += 1;
    let s = session_with(vec![account_entry(entry, FORK_LEDGER + 7)]);
    s.ledger().account(&late.account_id()).unwrap().unwrap();
    s.ledger().account(&alice()).unwrap().unwrap();
    let drift = s.ledger().drift();
    assert_eq!(drift.len(), 1);
    assert_eq!(drift[0].key, account_key(&late.account_id()));
    assert_eq!(drift[0].last_modified, FORK_LEDGER + 7);
    // Unknown keys are cached as absent after one fetch.
    let nobody = Ed25519Key::throwaway("nobody").account_id();
    assert!(s.ledger().account(&nobody).unwrap().is_none());
    let calls = s.ledger().stats().fetch_calls;
    assert!(s.ledger().account(&nobody).unwrap().is_none());
    assert_eq!(s.ledger().stats().fetch_calls, calls);
}

#[test]
fn the_host_protocol_must_match_the_network() {
    let mut fork = fork_point();
    fork.protocol_version -= 1;
    let err = Session::new(Box::new(base(vec![])), fork, SessionOptions::new(PASSPHRASE)).err().unwrap();
    assert!(err.to_string().contains("exact protocol parity"), "{err}");
}

#[test]
fn network_limits_load_from_the_ledger() {
    let s = session();
    let l = s.tx_limits();
    assert!(l.tx_max_instructions > 0 && l.tx_max_instructions < u64::MAX);
    assert!(l.tx_max_write_bytes > 0 && l.tx_max_footprint_entries > 0);
    assert!(s.ledger_info().min_persistent_entry_ttl > 0);
}

#[test]
fn substitution_keeps_sponsorship_descriptors_aligned_with_signers() {
    use soroban_sim_core::host::xdr::{
        AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext, AccountEntryExtensionV2,
        AccountEntryExtensionV2Ext, Liabilities, Signer, SignerKey, SponsorshipDescriptor, Uint256,
    };
    let dave_real = real_key("dave");
    let cosigner = real_key("dave-cosigner");
    let sponsor = real_key("sponsor").account_id();
    let mut dave = new_account(dave_real.account_id(), 100 * XLM, FORK_LEDGER - 1);
    dave.num_sub_entries = 1;
    dave.signers = vec![Signer { key: SignerKey::Ed25519(Uint256(cosigner.public_key())), weight: 1 }].try_into().unwrap();
    dave.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
        liabilities: Liabilities { buying: 0, selling: 0 },
        ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
            num_sponsored: 1,
            num_sponsoring: 0,
            signer_sponsoring_i_ds: vec![SponsorshipDescriptor(Some(sponsor.clone()))].try_into().unwrap(),
            ext: AccountEntryExtensionV2Ext::V0,
        }),
    });
    let s = session_with(vec![account_entry(dave, FORK_LEDGER - 1)]);
    let t1 = Ed25519Key::throwaway("dave");
    let t2 = Ed25519Key::throwaway("dave-cosigner");
    s.substitute_master_key(&dave_real.account_id(), &t1, "dave").unwrap();
    s.substitute_signer(&dave_real.account_id(), cosigner.public_key(), &t2, "dave cosigner").unwrap();
    let a = s.ledger().account(&dave_real.account_id()).unwrap().unwrap();
    let AccountEntryExt::V1(AccountEntryExtensionV1 { ext: AccountEntryExtensionV1Ext::V2(v2), .. }) = &a.ext else { panic!("v2 extension kept") };
    assert_eq!(a.signers.len(), 2);
    assert_eq!(v2.signer_sponsoring_i_ds.len(), 2);
    for (signer, sponsorship) in a.signers.iter().zip(v2.signer_sponsoring_i_ds.iter()) {
        let sponsored = signer.key == SignerKey::Ed25519(Uint256(t2.public_key()));
        assert_eq!(sponsorship.0.is_some(), sponsored, "the cosigner's sponsor follows it to its new key");
    }
    assert!(a.signers.windows(2).all(|w| w[0].key < w[1].key), "signers stay sorted");
}

#[test]
fn a_checkpoint_restores_writes_overrides_clock_and_log() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let add = add_contract(&mut s, &key);
    let at = s.checkpoint();
    let (ledger, time, balance_then, overrides) =
        (s.ledger_info().sequence_number, s.ledger_info().timestamp, balance(&s, &alice()), s.ledger().overrides().len());

    let first = prepare(&s, &alice(), std::slice::from_ref(&key), invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(2)]), &no_auth());
    s.submit("after the checkpoint", &first.envelope).unwrap();
    s.fund(&bob(), 10_000_000, "funding after the checkpoint").unwrap();
    s.advance(100, None);
    s.restore(&at);

    assert_eq!((s.ledger_info().sequence_number, s.ledger_info().timestamp), (ledger, time));
    assert_eq!(balance(&s, &alice()), balance_then);
    assert_eq!(s.ledger().overrides().len(), overrides);
    assert_eq!(s.log().len(), 2, "the upload and the deploy");
    // The undone transaction applies again, in the same ledger.
    let again = s.submit("replayed after the restore", &first.envelope).unwrap();
    assert!(again.status.is_applied(), "{}", again.status.summary());
    assert_eq!(again.ledger, ledger);
}
