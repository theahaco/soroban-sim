//! Classic operations and ledgers holding several transactions, against
//! stellar-core's result codes: CreateAccount, Payment (XLM and an issued
//! asset), SetOptions, ChangeTrust, multi-operation transactions, the
//! transaction set (one per source account, per-ledger limits, fee
//! priority), fees charged before anything applies, the checks core repeats
//! at apply, and frozen keys.

mod common;

use common::*;
use soroban_env_host::xdr::{
    AccountId, AlphaNum4, Asset, AssetCode4, ChangeTrustAsset, ChangeTrustOp, ChangeTrustResult, ConfigSettingEntry,
    CreateAccountOp, CreateAccountResult, EncodedLedgerKey, FreezeBypassTxs, FrozenLedgerKeys, Hash, LedgerEntry,
    LedgerEntryData, LedgerEntryExt, Limits, Memo, MuxedAccount, Operation, OperationBody, OperationResult,
    OperationResultTr, PaymentOp, PaymentResult, Preconditions, PublicKey, ScVal, SetOptionsOp, SetOptionsResult, Signer,
    SignerKey, String32, Thresholds, TrustLineEntry, Uint256, WriteXdr,
};
use soroban_sim_core::session::*;

// --- building transactions ---

fn op(body: OperationBody) -> Operation {
    Operation { source_account: None, body }
}

fn op_from(source: &AccountId, body: OperationBody) -> Operation {
    let PublicKey::PublicKeyTypeEd25519(k) = &source.0;
    Operation { source_account: Some(MuxedAccount::Ed25519(k.clone())), body }
}

fn create_account(dest: &AccountId, stroops: i64) -> OperationBody {
    OperationBody::CreateAccount(CreateAccountOp { destination: dest.clone(), starting_balance: stroops })
}

fn pay(dest: &AccountId, asset: Asset, amount: i64) -> OperationBody {
    let PublicKey::PublicKeyTypeEd25519(k) = &dest.0;
    OperationBody::Payment(PaymentOp { destination: MuxedAccount::Ed25519(k.clone()), asset, amount })
}

fn set_options(edit: impl FnOnce(&mut SetOptionsOp)) -> OperationBody {
    let mut o = SetOptionsOp {
        inflation_dest: None,
        clear_flags: None,
        set_flags: None,
        master_weight: None,
        low_threshold: None,
        med_threshold: None,
        high_threshold: None,
        home_domain: None,
        signer: None,
    };
    edit(&mut o);
    OperationBody::SetOptions(o)
}

fn domain(text: &str) -> String32 {
    String32(text.as_bytes().to_vec().try_into().unwrap())
}

fn signer(key: &Ed25519Key, weight: u32) -> Signer {
    Signer { key: SignerKey::Ed25519(Uint256(key.public_key())), weight }
}

fn usd(issuer: &AccountId) -> Asset {
    Asset::CreditAlphanum4(AlphaNum4 { asset_code: AssetCode4(*b"USD\0"), issuer: issuer.clone() })
}

fn trust(asset: &Asset, limit: i64) -> OperationBody {
    let Asset::CreditAlphanum4(a) = asset else { panic!("alphanum4") };
    OperationBody::ChangeTrust(ChangeTrustOp { line: ChangeTrustAsset::CreditAlphanum4(a.clone()), limit })
}

fn envelope(s: &Session, source: &AccountId, signers: &[Ed25519Key], ops: Vec<Operation>) -> soroban_env_host::xdr::TransactionEnvelope {
    envelope_with(s, source, signers, ops, None, Memo::None)
}

fn envelope_with(s: &Session, source: &AccountId, signers: &[Ed25519Key], ops: Vec<Operation>, sequence: Option<i64>, memo: Memo) -> soroban_env_host::xdr::TransactionEnvelope {
    s.prepare_classic(&ClassicRequest { source: source.clone(), operations: ops, envelope_signers: signers, memo, preconditions: Preconditions::None, sequence })
        .unwrap()
        .envelope
}

fn submit(s: &mut Session, label: &str, source: &AccountId, signers: &[Ed25519Key], ops: Vec<Operation>) -> TxRecord {
    let env = envelope(s, source, signers, ops);
    s.submit(label, &env).unwrap()
}

fn applied(r: &TxRecord) {
    assert!(r.status.is_applied(), "{}: {}", r.label, r.status.summary());
}

/// The record failed at apply with these operation results.
fn failed_with(r: &TxRecord, want: &[OperationResult]) {
    assert!(matches!(r.status, TxStatus::Failed { .. }), "{}: expected txFAILED, got {}", r.label, r.status.summary());
    assert_eq!(r.op_results, want, "{}", r.label);
}

/// Refused before inclusion as `txFAILED` with this result at `index`.
fn rejected_op(r: &TxRecord, code: TxCode, index: usize, want: OperationResult) {
    expect_rejected(r, code);
    assert_eq!(r.op_results.get(index), Some(&want), "{}: {:?}", r.label, r.op_results);
}

fn ca(r: CreateAccountResult) -> OperationResult {
    OperationResult::OpInner(OperationResultTr::CreateAccount(r))
}
fn pr(r: PaymentResult) -> OperationResult {
    OperationResult::OpInner(OperationResultTr::Payment(r))
}
fn so(r: SetOptionsResult) -> OperationResult {
    OperationResult::OpInner(OperationResultTr::SetOptions(r))
}
fn ct(r: ChangeTrustResult) -> OperationResult {
    OperationResult::OpInner(OperationResultTr::ChangeTrust(r))
}

fn account(s: &Session, id: &AccountId) -> soroban_env_host::xdr::AccountEntry {
    s.ledger().account(id).unwrap().unwrap()
}

fn line(s: &Session, id: &AccountId, asset: &Asset) -> Option<TrustLineEntry> {
    match s.ledger().get(&trustline_key(id, asset)).unwrap() {
        Some(StoredEntry { entry: LedgerEntry { data: LedgerEntryData::Trustline(t), .. }, .. }) => Some(t),
        _ => None,
    }
}

/// A rehearsed alice who funds `name`'s account (with `xlm`) through a real
/// CreateAccount, returning its throwaway key.
fn funded(s: &mut Session, alice_key: &Ed25519Key, name: &str, xlm: i64) -> Ed25519Key {
    let k = Ed25519Key::throwaway(name);
    applied(&submit(s, &format!("create {name}"), &alice(), std::slice::from_ref(alice_key), vec![op(create_account(&k.account_id(), xlm * XLM))]));
    k
}

// --- CreateAccount ---

#[test]
fn create_account_funds_a_new_account_the_way_core_does() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = Ed25519Key::throwaway("carol");
    let (bal, ledger) = (balance(&s, &alice()), s.ledger_info().sequence_number);
    let r = submit(&mut s, "create carol", &alice(), std::slice::from_ref(&key), vec![op(create_account(&carol.account_id(), 25 * XLM))]);
    applied(&r);
    assert_eq!(r.op_results, vec![ca(CreateAccountResult::Success)]);
    assert_eq!((r.fee_charged, r.refund, r.application_order), (100, 0, Some(1)));
    let c = account(&s, &carol.account_id());
    assert_eq!(c.balance, 25 * XLM);
    // core's getStartingSequenceNumber: the creating ledger, shifted 32 bits.
    assert_eq!(c.seq_num.0, (ledger as i64) << 32);
    assert_eq!(c.thresholds, Thresholds([1, 0, 0, 0]));
    assert_eq!(balance(&s, &alice()), bal - 25 * XLM - 100);
    // A real transaction, not an override: only alice's key substitution is logged.
    assert_eq!(s.ledger().overrides().len(), 1);
    assert!(s.ledger().diff().iter().any(|d| d.key == account_key(&carol.account_id()) && d.change() == Change::Created && !d.overridden));
}

#[test]
fn create_account_failures_carry_core_result_codes() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let k = std::slice::from_ref(&key);
    let carol = Ed25519Key::throwaway("carol").account_id();

    // An existing destination: included, fee and sequence number consumed.
    let (bal, sq) = (balance(&s, &alice()), seq(&s, &alice()));
    let r = submit(&mut s, "exists", &alice(), k, vec![op(create_account(&bob(), 5 * XLM))]);
    failed_with(&r, &[ca(CreateAccountResult::AlreadyExist)]);
    assert_eq!((balance(&s, &alice()), seq(&s, &alice())), (bal - 100, sq + 1));

    // Below two base reserves (1 XLM at a 0.5 XLM base reserve).
    failed_with(&submit(&mut s, "low reserve", &alice(), k, vec![op(create_account(&carol, XLM - 1))]), &[ca(CreateAccountResult::LowReserve)]);
    // More than alice has above her reserve.
    let everything = balance(&s, &alice());
    let r = submit(&mut s, "underfunded", &alice(), k, vec![op(create_account(&carol, everything))]);
    failed_with(&r, &[ca(CreateAccountResult::Underfunded)]);
    assert!(s.ledger().account(&carol).unwrap().is_none());

    // Malformed (creating yourself) is refused before inclusion: no fee.
    let (bal, sq) = (balance(&s, &alice()), seq(&s, &alice()));
    let r = submit(&mut s, "self", &alice(), k, vec![op(create_account(&alice(), 5 * XLM))]);
    rejected_op(&r, TxCode::OpFailed, 0, ca(CreateAccountResult::Malformed));
    assert!(r.status.summary().contains("CREATE_ACCOUNT_MALFORMED"), "{}", r.status.summary());
    assert_eq!((balance(&s, &alice()), seq(&s, &alice())), (bal, sq));
}

// --- Payment ---

#[test]
fn native_payments_respect_reserves_and_destinations() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let k = std::slice::from_ref(&key);
    let carol = funded(&mut s, &key, "carol", 10);

    let before = balance(&s, &carol.account_id());
    applied(&submit(&mut s, "pay carol", &alice(), k, vec![op(pay(&carol.account_id(), Asset::Native, 3 * XLM))]));
    assert_eq!(balance(&s, &carol.account_id()), before + 3 * XLM);

    // Paying yourself XLM succeeds without touching the balance.
    let bal = balance(&s, &alice());
    applied(&submit(&mut s, "self", &alice(), k, vec![op(pay(&alice(), Asset::Native, 1_000_000 * XLM))]));
    assert_eq!(balance(&s, &alice()), bal - 100);

    let nobody = Ed25519Key::throwaway("nobody").account_id();
    failed_with(&submit(&mut s, "no destination", &alice(), k, vec![op(pay(&nobody, Asset::Native, XLM))]), &[pr(PaymentResult::NoDestination)]);

    // carol holds 13 XLM, needs 1 XLM reserve and pays a 100-stroop fee.
    let c = carol.account_id();
    let all_but_reserve = balance(&s, &c) - 100 - XLM;
    failed_with(&submit(&mut s, "into the reserve", &c, std::slice::from_ref(&carol), vec![op(pay(&alice(), Asset::Native, all_but_reserve + 1))]), &[pr(PaymentResult::Underfunded)]);
    applied(&submit(&mut s, "down to the reserve", &c, std::slice::from_ref(&carol), vec![op(pay(&alice(), Asset::Native, all_but_reserve - 100))]));
    assert_eq!(balance(&s, &c), XLM);

    let r = submit(&mut s, "zero", &alice(), k, vec![op(pay(&c, Asset::Native, 0))]);
    rejected_op(&r, TxCode::OpFailed, 0, pr(PaymentResult::Malformed));
}

#[test]
fn issued_asset_payments_need_authorized_trustlines() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let issuer = funded(&mut s, &key, "issuer", 20);
    let holder = funded(&mut s, &key, "holder", 20);
    let other = funded(&mut s, &key, "other", 20);
    let (i, h, o) = (issuer.account_id(), holder.account_id(), other.account_id());
    let asset = usd(&i);

    // No trustline yet: the destination's check decides.
    failed_with(&submit(&mut s, "no trust", &i, std::slice::from_ref(&issuer), vec![op(pay(&h, asset.clone(), 5))]), &[pr(PaymentResult::NoTrust)]);
    let r = submit(&mut s, "holder trusts USD", &h, std::slice::from_ref(&holder), vec![op(trust(&asset, 1_000))]);
    applied(&r);
    assert_eq!(r.op_results, vec![ct(ChangeTrustResult::Success)]);
    let tl = line(&s, &h, &asset).unwrap();
    assert_eq!((tl.balance, tl.limit, tl.flags), (0, 1_000, 1));
    assert_eq!(account(&s, &h).num_sub_entries, 1);

    // The issuer mints from its unlimited line.
    applied(&submit(&mut s, "mint", &i, std::slice::from_ref(&issuer), vec![op(pay(&h, asset.clone(), 600))]));
    assert_eq!(line(&s, &h, &asset).unwrap().balance, 600);
    failed_with(&submit(&mut s, "over the limit", &i, std::slice::from_ref(&issuer), vec![op(pay(&h, asset.clone(), 401))]), &[pr(PaymentResult::LineFull)]);
    // A holder without a line cannot send; one that has too little cannot either.
    applied(&submit(&mut s, "other trusts USD", &o, std::slice::from_ref(&other), vec![op(trust(&asset, 1_000))]));
    failed_with(&submit(&mut s, "src no trust", &alice(), std::slice::from_ref(&key), vec![op(pay(&o, asset.clone(), 1))]), &[pr(PaymentResult::SrcNoTrust)]);
    failed_with(&submit(&mut s, "underfunded", &h, std::slice::from_ref(&holder), vec![op(pay(&o, asset.clone(), 601))]), &[pr(PaymentResult::Underfunded)]);
    applied(&submit(&mut s, "holder to other", &h, std::slice::from_ref(&holder), vec![op(pay(&o, asset.clone(), 100))]));
    // Paying the issuer back burns: no trustline needed on its side.
    applied(&submit(&mut s, "burn", &h, std::slice::from_ref(&holder), vec![op(pay(&i, asset.clone(), 500))]));
    assert_eq!((line(&s, &h, &asset).unwrap().balance, line(&s, &o, &asset).unwrap().balance), (0, 100));

    // An issuer that requires authorization: new lines start unauthorized.
    let strict = funded(&mut s, &key, "strict", 20);
    applied(&submit(&mut s, "auth required", &strict.account_id(), std::slice::from_ref(&strict), vec![op(set_options(|o| o.set_flags = Some(1)))]));
    let strict_usd = usd(&strict.account_id());
    applied(&submit(&mut s, "holder trusts strict", &h, std::slice::from_ref(&holder), vec![op(trust(&strict_usd, 50))]));
    assert_eq!(line(&s, &h, &strict_usd).unwrap().flags, 0);
    failed_with(&submit(&mut s, "not authorized", &strict.account_id(), std::slice::from_ref(&strict), vec![op(pay(&h, strict_usd, 1))]), &[pr(PaymentResult::NotAuthorized)]);
}

// --- SetOptions ---

#[test]
fn set_options_adds_a_cosigner_and_thresholds_that_later_envelopes_must_meet() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let admin = funded(&mut s, &key, "admin", 10);
    let cosigner = Ed25519Key::throwaway("cosigner");
    let a = admin.account_id();

    let r = submit(
        &mut s,
        "add co-signer",
        &a,
        std::slice::from_ref(&admin),
        vec![op(set_options(|o| {
            o.signer = Some(signer(&cosigner, 1));
            o.master_weight = Some(1);
            o.low_threshold = Some(1);
            o.med_threshold = Some(2);
            o.high_threshold = Some(2);
            o.home_domain = Some(domain("rehearsal.invalid"));
        }))],
    );
    applied(&r);
    assert_eq!(r.op_results, vec![so(SetOptionsResult::Success)]);
    let acc = account(&s, &a);
    assert_eq!(acc.thresholds, Thresholds([1, 1, 2, 2]));
    assert_eq!(acc.signers.as_slice(), &[signer(&cosigner, 1)]);
    assert_eq!(acc.num_sub_entries, 1);
    assert_eq!(acc.home_domain.to_string(), "rehearsal.invalid");

    // A payment needs the medium threshold now: the master alone is short.
    let r = submit(&mut s, "master alone", &a, std::slice::from_ref(&admin), vec![op(pay(&alice(), Asset::Native, XLM))]);
    rejected_op(&r, TxCode::OpBadAuth, 0, OperationResult::OpBadAuth);
    applied(&submit(&mut s, "both sign", &a, &[admin.clone(), cosigner.clone()], vec![op(pay(&alice(), Asset::Native, XLM))]));
    // The low threshold still lets the master alone pay the fee... for a
    // signature that is not needed, core says txBAD_AUTH_EXTRA.
    let stranger = Ed25519Key::throwaway("stranger");
    expect_rejected(&submit(&mut s, "extra signature", &a, &[admin.clone(), cosigner.clone(), stranger], vec![op(pay(&alice(), Asset::Native, XLM))]), TxCode::BadAuthExtra);

    // Removing the co-signer needs the high threshold (2), then frees its reserve.
    let r = submit(&mut s, "remove co-signer", &a, &[admin.clone(), cosigner.clone()], vec![op(set_options(|o| {
        o.signer = Some(signer(&cosigner, 0));
        o.med_threshold = Some(0);
        o.high_threshold = Some(0);
    }))]);
    applied(&r);
    let acc = account(&s, &a);
    assert!(acc.signers.is_empty());
    assert_eq!(acc.num_sub_entries, 0);
}

#[test]
fn set_options_failures_carry_core_result_codes() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let k = std::slice::from_ref(&key);

    // Malformed ones are refused before inclusion, fee-free.
    for (label, body, want) in [
        ("heavy signer", set_options(|o| o.signer = Some(signer(&Ed25519Key::throwaway("x"), 256))), SetOptionsResult::BadSigner),
        ("threshold 256", set_options(|o| o.med_threshold = Some(256)), SetOptionsResult::ThresholdOutOfRange),
        ("unknown flag", set_options(|o| o.set_flags = Some(16)), SetOptionsResult::UnknownFlag),
        ("set and clear", set_options(|o| {
            o.set_flags = Some(2);
            o.clear_flags = Some(2);
        }), SetOptionsResult::BadFlags),
        ("control char", set_options(|o| o.home_domain = Some(domain("bad\ndomain"))), SetOptionsResult::InvalidHomeDomain),
    ] {
        let r = submit(&mut s, label, &alice(), k, vec![op(body)]);
        rejected_op(&r, TxCode::OpFailed, 0, so(want));
    }
    // The account's own key cannot be added as a signer (the throwaway that
    // stands in for it is a signer already, but it is not alice's key).
    let r = submit(&mut s, "really self", &alice(), k, vec![op(set_options(|o| o.signer = Some(Signer { key: SignerKey::Ed25519(Uint256(real_key("alice").public_key())), weight: 1 })))]);
    rejected_op(&r, TxCode::OpFailed, 0, so(SetOptionsResult::BadSigner));

    // Apply-time failures are included and pay the fee.
    failed_with(&submit(&mut s, "clawback without revocable", &alice(), k, vec![op(set_options(|o| o.set_flags = Some(8)))]), &[so(SetOptionsResult::AuthRevocableRequired)]);
    let nobody = Ed25519Key::throwaway("nobody").account_id();
    failed_with(&submit(&mut s, "inflation to nobody", &alice(), k, vec![op(set_options(|o| o.inflation_dest = Some(nobody.clone())))]), &[so(SetOptionsResult::InvalidInflation)]);
    applied(&submit(&mut s, "immutable", &alice(), k, vec![op(set_options(|o| o.set_flags = Some(4)))]));
    failed_with(&submit(&mut s, "cant change", &alice(), k, vec![op(set_options(|o| o.clear_flags = Some(4)))]), &[so(SetOptionsResult::CantChange)]);

    // Twenty signers is the most an account holds.
    let poor = funded(&mut s, &key, "poor", 2);
    let p = poor.account_id();
    let mut signers = vec![];
    for n in 0..20 {
        signers.push(Ed25519Key::throwaway(&format!("signer {n}")));
    }
    // The 300-stroop fee leaves first; then 2 XLM + 1,000 stroops covers the
    // account's own reserve (1 XLM) and two signers, not a third.
    applied(&submit(&mut s, "top up poor a little", &alice(), k, vec![op(pay(&p, Asset::Native, 1_000))]));
    let r = submit(&mut s, "three signers", &p, std::slice::from_ref(&poor), (0..3).map(|n| op(set_options(|o| o.signer = Some(signer(&signers[n], 1))))).collect());
    failed_with(&r, &[so(SetOptionsResult::Success), so(SetOptionsResult::Success), so(SetOptionsResult::LowReserve)]);
    assert!(account(&s, &p).signers.is_empty(), "a failed transaction keeps none of its operations' writes");
    applied(&submit(&mut s, "top up poor", &alice(), k, vec![op(pay(&p, Asset::Native, 20 * XLM))]));
    applied(&submit(&mut s, "twenty signers", &p, std::slice::from_ref(&poor), (0..20).map(|n| op(set_options(|o| o.signer = Some(signer(&signers[n], 1))))).collect()));
    assert_eq!(account(&s, &p).num_sub_entries, 20);
    let one_more = Ed25519Key::throwaway("one more");
    failed_with(&submit(&mut s, "twenty-one", &p, std::slice::from_ref(&poor), vec![op(set_options(|o| o.signer = Some(signer(&one_more, 1))))]), &[so(SetOptionsResult::TooManySigners)]);
}

#[test]
fn signer_changes_need_the_high_threshold() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let x = funded(&mut s, &key, "x", 10);
    let co = Ed25519Key::throwaway("x co");
    applied(&submit(&mut s, "thresholds 1/1/2", &x.account_id(), std::slice::from_ref(&x), vec![op(set_options(|o| {
        o.signer = Some(signer(&co, 1));
        o.low_threshold = Some(1);
        o.med_threshold = Some(1);
        o.high_threshold = Some(2);
    }))]));
    // The home domain alone is a medium-threshold change...
    applied(&submit(&mut s, "home domain", &x.account_id(), std::slice::from_ref(&x), vec![op(set_options(|o| o.home_domain = Some(domain("x.invalid"))))]));
    // ...a signer change is high.
    let r = submit(&mut s, "signer alone", &x.account_id(), std::slice::from_ref(&x), vec![op(set_options(|o| o.signer = Some(signer(&co, 0))))]);
    rejected_op(&r, TxCode::OpBadAuth, 0, OperationResult::OpBadAuth);
    assert!(r.status.summary().contains("high threshold 2"), "{}", r.status.summary());
}

// --- ChangeTrust ---

#[test]
fn change_trust_opens_changes_and_closes_trustlines() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let issuer = funded(&mut s, &key, "issuer", 10);
    let holder = funded(&mut s, &key, "holder", 10);
    let (i, h) = (issuer.account_id(), holder.account_id());
    let hk = std::slice::from_ref(&holder);
    let asset = usd(&i);

    failed_with(&submit(&mut s, "new with limit 0", &h, hk, vec![op(trust(&asset, 0))]), &[ct(ChangeTrustResult::InvalidLimit)]);
    let ghost = usd(&Ed25519Key::throwaway("ghost issuer").account_id());
    failed_with(&submit(&mut s, "no issuer", &h, hk, vec![op(trust(&ghost, 10))]), &[ct(ChangeTrustResult::NoIssuer)]);
    applied(&submit(&mut s, "open", &h, hk, vec![op(trust(&asset, 100))]));
    applied(&submit(&mut s, "mint", &i, std::slice::from_ref(&issuer), vec![op(pay(&h, asset.clone(), 40))]));
    failed_with(&submit(&mut s, "below the balance", &h, hk, vec![op(trust(&asset, 39))]), &[ct(ChangeTrustResult::InvalidLimit)]);
    applied(&submit(&mut s, "lower the limit", &h, hk, vec![op(trust(&asset, 40))]));
    assert_eq!(line(&s, &h, &asset).unwrap().limit, 40);
    failed_with(&submit(&mut s, "close with a balance", &h, hk, vec![op(trust(&asset, 0))]), &[ct(ChangeTrustResult::InvalidLimit)]);
    applied(&submit(&mut s, "pay it back", &h, hk, vec![op(pay(&i, asset.clone(), 40))]));
    applied(&submit(&mut s, "close", &h, hk, vec![op(trust(&asset, 0))]));
    assert!(line(&s, &h, &asset).is_none());
    assert_eq!(account(&s, &h).num_sub_entries, 0);

    // Malformed: trusting your own asset, or XLM.
    let r = submit(&mut s, "own asset", &i, std::slice::from_ref(&issuer), vec![op(trust(&asset, 10))]);
    rejected_op(&r, TxCode::OpFailed, 0, ct(ChangeTrustResult::Malformed));
    let native = OperationBody::ChangeTrust(ChangeTrustOp { line: ChangeTrustAsset::Native, limit: 10 });
    rejected_op(&submit(&mut s, "xlm", &h, hk, vec![op(native)]), TxCode::OpFailed, 0, ct(ChangeTrustResult::Malformed));

    // A trustline costs a base reserve.
    let broke = funded(&mut s, &key, "broke", 1);
    applied(&submit(&mut s, "top up a fee's worth", &alice(), std::slice::from_ref(&key), vec![op(pay(&broke.account_id(), Asset::Native, 1_000))]));
    failed_with(&submit(&mut s, "low reserve", &broke.account_id(), std::slice::from_ref(&broke), vec![op(trust(&asset, 10))]), &[ct(ChangeTrustResult::LowReserve)]);
}

// --- several operations, several sources ---

#[test]
fn a_failing_operation_rolls_back_the_whole_transaction_but_every_result_is_reported() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = Ed25519Key::throwaway("carol").account_id();
    let nobody = Ed25519Key::throwaway("nobody").account_id();
    let bal = balance(&s, &alice());
    let r = submit(
        &mut s,
        "create, then pay nobody, then pay carol",
        &alice(),
        std::slice::from_ref(&key),
        vec![op(create_account(&carol, 5 * XLM)), op(pay(&nobody, Asset::Native, XLM)), op(pay(&carol, Asset::Native, XLM))],
    );
    failed_with(&r, &[ca(CreateAccountResult::Success), pr(PaymentResult::NoDestination), pr(PaymentResult::Success)]);
    // Three operations: three base fees, and nothing else changed.
    assert_eq!(r.fee_charged, 300);
    assert_eq!(balance(&s, &alice()), bal - 300);
    assert!(s.ledger().account(&carol).unwrap().is_none());
}

#[test]
fn operations_sourced_from_other_accounts_need_their_signatures() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = funded(&mut s, &key, "carol", 10);
    let c = carol.account_id();
    // alice pays the fee; carol's operation needs carol's signature.
    let r = submit(&mut s, "carol unsigned", &alice(), std::slice::from_ref(&key), vec![op_from(&c, pay(&alice(), Asset::Native, XLM))]);
    rejected_op(&r, TxCode::OpBadAuth, 0, OperationResult::OpBadAuth);
    let bal = balance(&s, &c);
    applied(&submit(&mut s, "carol signs", &alice(), &[key.clone(), carol.clone()], vec![op_from(&c, pay(&alice(), Asset::Native, XLM))]));
    assert_eq!(balance(&s, &c), bal - XLM, "carol pays the XLM, alice the fee");

    // An account created earlier in the same transaction can source a later
    // operation: its key signs although it does not exist yet.
    let dave = Ed25519Key::throwaway("dave");
    let d = dave.account_id();
    let r = submit(
        &mut s,
        "create dave, dave sets a home domain",
        &alice(),
        &[key.clone(), dave.clone()],
        vec![op(create_account(&d, 5 * XLM)), op_from(&d, set_options(|o| o.home_domain = Some(domain("dave.invalid"))))],
    );
    applied(&r);
    assert_eq!(account(&s, &d).home_domain.to_string(), "dave.invalid");
    // Unsigned by dave, the same is refused; sourced from nobody, it fails.
    let erin = Ed25519Key::throwaway("erin").account_id();
    let r = submit(&mut s, "erin unsigned", &alice(), std::slice::from_ref(&key), vec![op(create_account(&erin, 5 * XLM)), op_from(&erin, set_options(|_| {}))]);
    rejected_op(&r, TxCode::OpBadAuth, 1, OperationResult::OpBadAuth);
}

// --- ledgers with several transactions ---

#[test]
fn transactions_from_different_sources_share_a_ledger() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = funded(&mut s, &key, "carol", 10);
    let dave = funded(&mut s, &key, "dave", 10);
    let ledger = s.ledger_info().sequence_number;
    let a = envelope(&s, &carol.account_id(), std::slice::from_ref(&carol), vec![op(pay(&alice(), Asset::Native, XLM))]);
    let b = envelope(&s, &dave.account_id(), std::slice::from_ref(&dave), vec![op(pay(&alice(), Asset::Native, XLM))]);
    let records = s.submit_ledger(&[("carol pays", &a), ("dave pays", &b)]).unwrap();
    assert_eq!(records.len(), 2);
    for r in &records {
        applied(r);
        assert_eq!(r.ledger, ledger);
        assert!(r.deferred.is_none());
    }
    let mut orders: Vec<u32> = records.iter().map(|r| r.application_order.unwrap()).collect();
    orders.sort();
    assert_eq!(orders, vec![1, 2]);
    // One ledger closed for both.
    assert_eq!(s.ledger_info().sequence_number, ledger + 1);
}

#[test]
fn a_second_transaction_from_the_same_source_waits_for_the_next_ledger() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let sq = seq(&s, &alice());
    let ledger = s.ledger_info().sequence_number;
    let carol = Ed25519Key::throwaway("carol").account_id();
    let first = envelope_with(&s, &alice(), std::slice::from_ref(&key), vec![op(create_account(&carol, 5 * XLM))], Some(sq + 1), Memo::None);
    let second = envelope_with(&s, &alice(), std::slice::from_ref(&key), vec![op(pay(&carol, Asset::Native, XLM))], Some(sq + 2), Memo::None);
    let records = s.submit_ledger(&[("create", &first), ("pay", &second)]).unwrap();
    applied(&records[0]);
    applied(&records[1]);
    assert_eq!((records[0].ledger, records[1].ledger), (ledger, ledger + 1));
    assert!(records[1].deferred.as_deref().unwrap().contains("one per account"), "{:?}", records[1].deferred);
    assert_eq!(balance(&s, &carol), 6 * XLM);
}

#[test]
fn per_ledger_limits_defer_the_lowest_fee_rate() {
    let mut options = SessionOptions::new(PASSPHRASE);
    let mut limits = LedgerLimits::unlimited();
    limits.max_classic_ops = 2;
    options.ledger_limits = Some(limits);
    let mut s = Session::new(Box::new(base(vec![])), fork_point(), options).unwrap();
    let key = rehearse_alice(&s);
    let keys: Vec<Ed25519Key> = ["c1", "c2", "c3"].iter().map(|n| funded(&mut s, &key, n, 10)).collect();
    let ledger = s.ledger_info().sequence_number;
    let mut envs = vec![];
    for (i, k) in keys.iter().enumerate() {
        let mut env = envelope(&s, &k.account_id(), std::slice::from_ref(k), vec![op(pay(&alice(), Asset::Native, XLM))]);
        if i == 2 {
            // The last one bids more, so it goes first.
            env = resign(&env, std::slice::from_ref(k), |tx| tx.fee = 1_000);
        }
        envs.push(env);
    }
    let records = s.submit_ledger(&[("c1", &envs[0]), ("c2", &envs[1]), ("c3", &envs[2])]).unwrap();
    let ledgers: Vec<u32> = records.iter().map(|r| r.ledger).collect();
    assert_eq!(ledgers, vec![ledger, ledger + 1, ledger]);
    assert!(records[1].deferred.as_deref().unwrap().contains("classic phase was full"));
    // No surge pricing: the high bidder still pays the base fee.
    assert_eq!(records[2].fee_charged, 100);
    // A transaction no ledger can hold is refused.
    let big = envelope(&s, &keys[0].account_id(), std::slice::from_ref(&keys[0]), (0..3).map(|_| op(pay(&alice(), Asset::Native, 1))).collect());
    let r = s.submit("three operations", &big).unwrap();
    assert!(matches!(r.status, TxStatus::Rejected { .. }) && r.status.summary().contains("per-ledger maximum 2"), "{}", r.status.summary());
}

#[test]
fn soroban_transactions_count_against_the_soroban_phase() {
    let mut options = SessionOptions::new(PASSPHRASE);
    let mut limits = LedgerLimits::unlimited();
    limits.soroban.txs = 1;
    options.ledger_limits = Some(limits);
    let mut s = Session::new(Box::new(base(vec![])), fork_point(), options).unwrap();
    let key = rehearse_alice(&s);
    let carol = funded(&mut s, &key, "carol", 100);
    let add = deploy(&mut s, &alice(), &key, "add_i32", "add", vec![]);
    let ledger = s.ledger_info().sequence_number;
    let call = |s: &Session, who: &AccountId, k: &Ed25519Key| prepare(s, who, std::slice::from_ref(k), invoke(&add, "add", vec![ScVal::I32(1), ScVal::I32(2)]), &no_auth()).envelope;
    let a = call(&s, &alice(), &key);
    let b = call(&s, &carol.account_id(), &carol);
    // A classic payment rides along in the classic phase.
    let c = envelope(&s, &bob(), &[real_key("bob"), real_key("bob-cosigner")], vec![op(pay(&alice(), Asset::Native, 1))]);
    let records = s.submit_ledger(&[("alice adds", &a), ("carol adds", &b), ("bob pays", &c)]).unwrap();
    for r in &records {
        applied(r);
    }
    assert_eq!(records.iter().map(|r| r.ledger).collect::<Vec<_>>(), vec![ledger, ledger + 1, ledger]);
    // The classic phase applies before the Soroban phase.
    assert_eq!((records[2].application_order, records[0].application_order), (Some(1), Some(2)));
    assert!(records[1].deferred.as_deref().unwrap().contains("Soroban phase was full"));
}

#[test]
fn every_fee_is_charged_before_any_transaction_applies() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = funded(&mut s, &key, "carol", 50);
    let c = carol.account_id();
    // carol's whole spendable balance, as the last closed ledger had it...
    let spendable = available_balance(&account(&s, &c), s.ledger_info().base_reserve);
    // ...moved out by alice's transaction (carol signs her operation), while
    // carol submits a transaction of her own for the same ledger.
    let drain = envelope(&s, &alice(), &[key.clone(), carol.clone()], vec![op_from(&c, pay(&alice(), Asset::Native, spendable))]);
    let own = envelope(&s, &c, std::slice::from_ref(&carol), vec![op(set_options(|o| o.home_domain = Some(domain("c.invalid"))))]);
    let records = s.submit_ledger(&[("drain carol", &drain), ("carol's own", &own)]).unwrap();
    // carol's fee left first, whatever the order: the drain is short by it.
    failed_with(&records[0], &[pr(PaymentResult::Underfunded)]);
    applied(&records[1]);
    assert_eq!(records[0].ledger, records[1].ledger);
}

#[test]
fn the_checks_core_repeats_at_apply_see_earlier_transactions_of_the_ledger() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = funded(&mut s, &key, "carol", 50);
    let dave = funded(&mut s, &key, "dave", 50);
    let c = carol.account_id();
    let cp = s.checkpoint();
    // carol's own transaction raises her medium threshold; dave's pays from
    // carol with carol's master key alone. Find a memo that applies carol's
    // first (the apply order is a shuffle by hash).
    for id in 0..64u64 {
        s.restore(&cp);
        let harden = envelope(&s, &c, std::slice::from_ref(&carol), vec![op(set_options(|o| o.med_threshold = Some(5)))]);
        let spend = envelope_with(&s, &dave.account_id(), &[dave.clone(), carol.clone()], vec![op_from(&c, pay(&dave.account_id(), Asset::Native, XLM))], None, Memo::Id(id));
        let records = s.submit_ledger(&[("harden", &harden), ("spend", &spend)]).unwrap();
        applied(&records[0]);
        if records[0].application_order > records[1].application_order {
            applied(&records[1]);
            continue;
        }
        // Valid when the set was built; refused at apply, after its fee.
        let r = &records[1];
        assert_eq!(r.failed_code, Some(TxCode::OpBadAuth), "{}", r.status.summary());
        assert_eq!(r.op_results, vec![OperationResult::OpBadAuth]);
        assert_eq!(r.fee_charged, 100);
        assert!(r.status.summary().contains("at apply"));
        return;
    }
    panic!("no memo put carol's transaction first");
}

// --- frozen keys (CAP-77) ---

fn config(entry: ConfigSettingEntry) -> StoredEntry {
    StoredEntry { entry: LedgerEntry { last_modified_ledger_seq: 1, data: LedgerEntryData::ConfigSetting(entry), ext: LedgerEntryExt::V0 }, live_until: None }
}

#[test]
fn frozen_keys_refuse_transactions_that_touch_them() {
    let frozen = account_key(&bob());
    let encoded = EncodedLedgerKey(frozen.to_xdr(Limits::none()).unwrap().try_into().unwrap());
    let mut s = session_with(vec![config(ConfigSettingEntry::FrozenLedgerKeys(FrozenLedgerKeys { keys: vec![encoded.clone()].try_into().unwrap() }))]);
    assert!(s.frozen_keys().keys.contains(&frozen));
    let key = rehearse_alice(&s);
    let r = submit(&mut s, "pay bob", &alice(), std::slice::from_ref(&key), vec![op(pay(&bob(), Asset::Native, XLM))]);
    expect_rejected(&r, TxCode::FrozenKeyAccessed);
    // bob cannot even pay a fee.
    let r = submit(&mut s, "bob pays", &bob(), &[real_key("bob"), real_key("bob-cosigner")], vec![op(pay(&alice(), Asset::Native, 1))]);
    expect_rejected(&r, TxCode::FrozenKeyAccessed);
    // Untouched keys are fine.
    applied(&submit(&mut s, "pay self", &alice(), std::slice::from_ref(&key), vec![op(pay(&alice(), Asset::Native, 1))]));

    // A listed transaction may bypass the freeze.
    let fresh = session();
    let key = rehearse_alice(&fresh);
    let env = envelope(&fresh, &alice(), std::slice::from_ref(&key), vec![op(pay(&bob(), Asset::Native, XLM))]);
    let soroban_env_host::xdr::TransactionEnvelope::Tx(e) = &env else { panic!() };
    let hash = transaction_hash(&e.tx, network_id(PASSPHRASE)).unwrap();
    let mut s2 = session_with(vec![
        config(ConfigSettingEntry::FrozenLedgerKeys(FrozenLedgerKeys { keys: vec![encoded].try_into().unwrap() })),
        config(ConfigSettingEntry::FreezeBypassTxs(FreezeBypassTxs { tx_hashes: vec![Hash(hash)].try_into().unwrap() })),
    ]);
    rehearse_alice(&s2);
    applied(&s2.submit("bypass", &env).unwrap());
}

/// A base that cannot answer for one key: an RPC outage mid-close.
struct Outage {
    inner: StaticSource,
    poison: soroban_env_host::xdr::LedgerKey,
}

impl LedgerSource for Outage {
    fn fetch(&self, keys: &[soroban_env_host::xdr::LedgerKey]) -> anyhow::Result<FetchedBatch> {
        if keys.contains(&self.poison) {
            anyhow::bail!("the base is unreachable");
        }
        self.inner.fetch(keys)
    }

    fn describe(&self) -> String {
        "a base with an outage".into()
    }
}

#[test]
fn an_error_mid_close_leaves_the_session_as_it_was() {
    use soroban_env_host::xdr::{LedgerKey, LedgerKeyContractCode, TransactionExt};
    let poison = LedgerKey::ContractCode(LedgerKeyContractCode { hash: Hash([9; 32]) });
    let mut s = Session::new(Box::new(Outage { inner: base(vec![]), poison: poison.clone() }), fork_point(), SessionOptions::new(PASSPHRASE)).unwrap();
    let alice_key = rehearse_alice(&s);
    let bob_key = Ed25519Key::throwaway("bob");
    s.substitute_master_key(&bob(), &bob_key, "bob").unwrap();
    let before = (balance(&s, &alice()), seq(&s, &alice()), balance(&s, &bob()), seq(&s, &bob()), s.ledger_info().sequence_number);

    let payment = envelope(&s, &bob(), &[bob_key, real_key("bob-cosigner")], vec![op(pay(&alice(), Asset::Native, XLM))]);
    let upload = prepare(&s, &alice(), std::slice::from_ref(&alice_key), upload(&wasm("add_i32")), &no_auth()).envelope;
    let upload = resign(&upload, &[alice_key], |tx| {
        tx.fee += 1_000_000;
        let TransactionExt::V1(data) = &mut tx.ext else { panic!("soroban data") };
        data.resource_fee += 1_000_000;
        let mut read_only = data.resources.footprint.read_only.to_vec();
        read_only.push(poison);
        data.resources.footprint.read_only = read_only.try_into().unwrap();
    });

    let err = s.submit_ledger(&[("pay", &payment), ("upload", &upload)]).unwrap_err();
    assert!(format!("{err:#}").contains("unreachable"), "{err:#}");
    // The payment applied and both fees were charged before the upload hit
    // the outage; all of it is undone.
    assert_eq!((balance(&s, &alice()), seq(&s, &alice()), balance(&s, &bob()), seq(&s, &bob()), s.ledger_info().sequence_number), before);
    assert_eq!(s.ledger().depth(), 0);
    assert!(s.log().is_empty());

    let r = s.submit("pay again", &payment).unwrap();
    assert!(matches!(r.status, TxStatus::Applied { .. }), "{}", r.status.summary());
    assert_eq!(balance(&s, &alice()), before.0 + XLM);
}

#[test]
fn an_error_in_a_later_ledger_undoes_the_whole_batch() {
    use soroban_env_host::xdr::{LedgerKey, LedgerKeyContractCode, SequenceNumber, TransactionExt};
    let poison = LedgerKey::ContractCode(LedgerKeyContractCode { hash: Hash([9; 32]) });
    let mut s = Session::new(Box::new(Outage { inner: base(vec![]), poison: poison.clone() }), fork_point(), SessionOptions::new(PASSPHRASE)).unwrap();
    let alice_key = rehearse_alice(&s);
    let before = (balance(&s, &alice()), seq(&s, &alice()), balance(&s, &bob()), s.ledger_info().sequence_number);

    let payment = envelope(&s, &alice(), std::slice::from_ref(&alice_key), vec![op(pay(&bob(), Asset::Native, XLM))]);
    let upload = prepare(&s, &alice(), std::slice::from_ref(&alice_key), upload(&wasm("add_i32")), &no_auth()).envelope;
    let upload = resign(&upload, &[alice_key], |tx| {
        tx.seq_num = SequenceNumber(tx.seq_num.0 + 1);
        tx.fee += 1_000_000;
        let TransactionExt::V1(data) = &mut tx.ext else { panic!("soroban data") };
        data.resource_fee += 1_000_000;
        let mut read_only = data.resources.footprint.read_only.to_vec();
        read_only.push(poison);
        data.resources.footprint.read_only = read_only.try_into().unwrap();
    });

    // One transaction per source per ledger: the payment closes ledger N,
    // the upload waits for N+1 and hits the outage there.
    let err = s.submit_ledger(&[("pay", &payment), ("upload", &upload)]).unwrap_err();
    assert!(format!("{err:#}").contains("unreachable"), "{err:#}");
    assert_eq!((balance(&s, &alice()), seq(&s, &alice()), balance(&s, &bob()), s.ledger_info().sequence_number), before);
    assert_eq!(s.ledger().depth(), 0);
    assert!(s.log().is_empty());

    let r = s.submit("pay again", &payment).unwrap();
    assert!(matches!(r.status, TxStatus::Applied { .. }), "{}", r.status.summary());
    assert_eq!(balance(&s, &bob()), before.2 + XLM);
}

// --- fee bumps ---

/// `inner` fee-bumped by `payer` (signed by `key`) with an outer fee of `fee`.
fn fee_bump(inner: &soroban_env_host::xdr::TransactionEnvelope, payer: &AccountId, key: &Ed25519Key, fee: i64) -> soroban_env_host::xdr::TransactionEnvelope {
    use soroban_env_host::xdr::{FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt, FeeBumpTransactionInnerTx, TransactionEnvelope};
    let TransactionEnvelope::Tx(e) = inner else { panic!("v1 envelope") };
    let PublicKey::PublicKeyTypeEd25519(k) = &payer.0;
    let tx = FeeBumpTransaction { fee_source: MuxedAccount::Ed25519(k.clone()), fee, inner_tx: FeeBumpTransactionInnerTx::Tx(e.clone()), ext: FeeBumpTransactionExt::V0 };
    let hash = fee_bump_hash(&tx, network_id(PASSPHRASE)).unwrap();
    TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope { tx, signatures: vec![key.sign_decorated(&hash)].try_into().unwrap() })
}

#[test]
fn a_fee_bumped_classic_transaction_is_paid_by_its_fee_source() {
    use soroban_env_host::xdr::{InnerTransactionResultResult, TransactionResultResult};
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = funded(&mut s, &key, "carol", 10);
    let dave = Ed25519Key::throwaway("dave").account_id();
    let c = carol.account_id();
    let (alice_before, carol_before, carol_seq) = (balance(&s, &alice()), balance(&s, &c), seq(&s, &c));

    // carol creates dave; alice pays the fee: 100 per operation, plus one for the bump.
    let inner = envelope(&s, &c, std::slice::from_ref(&carol), vec![op(create_account(&dave, 2 * XLM))]);
    let r = s.submit("bumped create", &fee_bump(&inner, &alice(), &key, 200)).unwrap();
    applied(&r);
    assert_eq!((r.fee_charged, r.fee_source.as_ref(), r.source.as_ref()), (200, Some(&alice()), Some(&c)));
    assert_eq!(balance(&s, &alice()), alice_before - 200);
    assert_eq!(balance(&s, &c), carol_before - 2 * XLM, "carol pays no fee");
    assert_eq!(seq(&s, &c), carol_seq + 1, "the inner sequence is consumed");
    assert!(s.ledger().account(&dave).unwrap().is_some());
    let TransactionResultResult::TxFeeBumpInnerSuccess(pair) = r.transaction_result().result else { panic!("{:?}", r.transaction_result()) };
    assert_eq!(pair.transaction_hash.0, r.inner_hash.unwrap());
    let InnerTransactionResultResult::TxSuccess(ops) = pair.result.result else { panic!() };
    assert_eq!(ops.to_vec(), vec![ca(CreateAccountResult::Success)]);

    // An inner operation that fails at apply: txFEE_BUMP_INNER_FAILED with
    // the operation's code, and the fee source still pays.
    let inner = envelope(&s, &c, std::slice::from_ref(&carol), vec![op(pay(&alice(), Asset::Native, 100 * XLM))]);
    let r = s.submit("bumped overdraft", &fee_bump(&inner, &alice(), &key, 200)).unwrap();
    failed_with(&r, &[pr(PaymentResult::Underfunded)]);
    let TransactionResultResult::TxFeeBumpInnerFailed(pair) = r.transaction_result().result else { panic!("{:?}", r.transaction_result()) };
    let InnerTransactionResultResult::TxFailed(ops) = pair.result.result else { panic!() };
    assert_eq!(ops.to_vec(), vec![pr(PaymentResult::Underfunded)]);
    assert_eq!(r.fee_charged, 200);

    // An inner transaction carol did not sign is refused, fee-free.
    let unsigned = envelope(&s, &c, &[Ed25519Key::throwaway("not carol")], vec![op(pay(&alice(), Asset::Native, 1))]);
    let r = s.submit("bumped forgery", &fee_bump(&unsigned, &alice(), &key, 200)).unwrap();
    assert!(matches!(r.status, TxStatus::Rejected { code: TxCode::BadAuth, fee_bump_inner: true, .. }), "{}", r.status.summary());
    assert!(r.status.summary().starts_with("txFEE_BUMP_INNER_FAILED(txBAD_AUTH)"), "{}", r.status.summary());
    // The outer fee must cover one more operation than the inner transaction.
    let inner = envelope(&s, &c, std::slice::from_ref(&carol), vec![op(pay(&alice(), Asset::Native, 1))]);
    expect_rejected(&s.submit("bumped too cheaply", &fee_bump(&inner, &alice(), &key, 199)).unwrap(), TxCode::InsufficientFee);
}

#[test]
fn fee_bumps_share_a_ledger_and_count_one_more_operation() {
    let mut options = SessionOptions::new(PASSPHRASE);
    let mut limits = LedgerLimits::unlimited();
    limits.max_classic_ops = 2;
    options.ledger_limits = Some(limits);
    let mut s = Session::new(Box::new(base(vec![])), fork_point(), options).unwrap();
    let key = rehearse_alice(&s);
    let carol = funded(&mut s, &key, "carol", 10);
    let dave = funded(&mut s, &key, "dave", 10);
    let (c, d) = (carol.account_id(), dave.account_id());
    let ledger = s.ledger_info().sequence_number;
    let alice_before = balance(&s, &alice());
    // alice pays for carol's and dave's payments: two transactions from two
    // sources, one fee source. Each fee bump counts two operations, so the
    // classic phase (2 operations) takes one and the other waits.
    let a = fee_bump(&envelope(&s, &c, std::slice::from_ref(&carol), vec![op(pay(&alice(), Asset::Native, XLM))]), &alice(), &key, 200);
    let b = fee_bump(&envelope(&s, &d, std::slice::from_ref(&dave), vec![op(pay(&alice(), Asset::Native, XLM))]), &alice(), &key, 200);
    let records = s.submit_ledger(&[("carol, paid by alice", &a), ("dave, paid by alice", &b)]).unwrap();
    applied(&records[0]);
    applied(&records[1]);
    assert_eq!((records[0].ledger, records[1].ledger), (ledger, ledger + 1));
    assert!(records[1].deferred.as_deref().unwrap().contains("classic phase was full (4 operations > 2)"), "{:?}", records[1].deferred);
    assert_eq!(balance(&s, &alice()), alice_before - 400 + 2 * XLM);
}

/// A fresh account holding `spare` stroops above its reserve, created by alice.
fn barely_funded(s: &mut Session, alice_key: &Ed25519Key, name: &str, spare: i64) -> Ed25519Key {
    let k = Ed25519Key::throwaway(name);
    let start = 2 * s.ledger_info().base_reserve as i64 + spare;
    applied(&submit(s, &format!("create {name}"), &alice(), std::slice::from_ref(alice_key), vec![op(create_account(&k.account_id(), start))]));
    k
}

#[test]
fn a_fee_source_cannot_bid_more_than_it_has_across_a_ledger() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = funded(&mut s, &key, "carol", 10);
    let dave = funded(&mut s, &key, "dave", 10);
    let frank = barely_funded(&mut s, &key, "frank", 250);
    let (c, d, f) = (carol.account_id(), dave.account_id(), frank.account_id());
    let (frank_before, dave_seq) = (balance(&s, &f), seq(&s, &d));
    // frank can afford one 200-stroop bid, not two: the second in
    // submission order is refused, as core's queue refuses it.
    let a = fee_bump(&envelope(&s, &c, std::slice::from_ref(&carol), vec![op(pay(&alice(), Asset::Native, XLM))]), &f, &frank, 200);
    let b = fee_bump(&envelope(&s, &d, std::slice::from_ref(&dave), vec![op(pay(&alice(), Asset::Native, XLM))]), &f, &frank, 200);
    let records = s.submit_ledger(&[("carol, paid by frank", &a), ("dave, paid by frank", &b)]).unwrap();
    applied(&records[0]);
    expect_rejected(&records[1], TxCode::InsufficientBalance);
    assert!(records[1].status.summary().contains(&format!("fee source {f}")), "{}", records[1].status.summary());
    assert_eq!(balance(&s, &f), frank_before - 200);
    assert_eq!(seq(&s, &d), dave_seq, "dave's transaction never applied");

    // erin's own transaction (100) and a fee bump erin pays (200) are 300
    // against her 250: the fee bump, sent second, is refused.
    let erin = barely_funded(&mut s, &key, "erin", 250);
    let e = erin.account_id();
    let (erin_before, dave_seq) = (balance(&s, &e), seq(&s, &d));
    let own = envelope(&s, &e, std::slice::from_ref(&erin), vec![op(set_options(|o| o.home_domain = Some(domain("e.invalid"))))]);
    let bumped = fee_bump(&envelope(&s, &d, std::slice::from_ref(&dave), vec![op(pay(&alice(), Asset::Native, XLM))]), &e, &erin, 200);
    let records = s.submit_ledger(&[("erin's own", &own), ("dave, paid by erin", &bumped)]).unwrap();
    applied(&records[0]);
    expect_rejected(&records[1], TxCode::InsufficientBalance);
    assert_eq!(seq(&s, &d), dave_seq);
    assert_eq!(balance(&s, &e), erin_before - 100);
}

#[test]
fn a_transaction_no_ledger_can_hold_does_not_count_against_its_fee_source() {
    let mut options = SessionOptions::new(PASSPHRASE);
    let mut limits = LedgerLimits::unlimited();
    limits.max_classic_ops = 2;
    options.ledger_limits = Some(limits);
    let mut s = Session::new(Box::new(base(vec![])), fork_point(), options).unwrap();
    let key = rehearse_alice(&s);
    let carol = funded(&mut s, &key, "carol", 10);
    let dave = funded(&mut s, &key, "dave", 10);
    let (c, d) = (carol.account_id(), dave.account_id());
    // frank can pay 400: the 300 bid of a fee bump no ledger can hold (two
    // operations plus the bump, over the maximum of 2), or the 200 of one
    // that fits, not both.
    let frank = barely_funded(&mut s, &key, "frank", 400);
    let f = frank.account_id();
    let two_ops = envelope(&s, &c, std::slice::from_ref(&carol), (0..2).map(|_| op(pay(&alice(), Asset::Native, 1))).collect());
    let too_big = fee_bump(&two_ops, &f, &frank, 300);
    let fits = fee_bump(&envelope(&s, &d, std::slice::from_ref(&dave), vec![op(pay(&alice(), Asset::Native, XLM))]), &f, &frank, 200);
    let frank_before = balance(&s, &f);
    let records = s.submit_ledger(&[("too big, paid by frank", &too_big), ("dave, paid by frank", &fits)]).unwrap();
    expect_rejected(&records[0], TxCode::SorobanInvalid);
    assert!(records[0].status.summary().contains("per-ledger maximum 2"), "{}", records[0].status.summary());
    applied(&records[1]);
    assert_eq!(balance(&s, &f), frank_before - 200);
}
