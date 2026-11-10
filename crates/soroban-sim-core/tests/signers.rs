//! Signer adapters, end to end: each one signs an auth entry that the fixture
//! smart account (an OZ-shaped `__check_auth`) or the host's G-account check
//! verifies in enforcing mode, both in `prepare` and at apply.

mod common;

use common::*;
use soroban_sim_core::host::xdr::{Int128Parts, ScAddress, ScBytes, ScSymbol, ScVal, ScVec, SorobanCredentials};
use soroban_sim_core::session::*;
use std::collections::BTreeMap;

const ED25519: u32 = 0;
const SECP256R1: u32 = 1;
const WEBAUTHN: u32 = 2;
const RULE_IDS: u32 = 0;
const ADDR_OZ_V1: u32 = 1;

fn sym(s: &str) -> ScVal {
    ScVal::Symbol(ScSymbol(s.try_into().unwrap()))
}

fn bytes(b: &[u8]) -> ScVal {
    ScVal::Bytes(ScBytes(b.to_vec().try_into().unwrap()))
}

fn vec(v: Vec<ScVal>) -> ScVal {
    ScVal::Vec(Some(ScVec(v.try_into().unwrap())))
}

fn external(verifier: &ScAddress, key: &ExternalKey) -> ScVal {
    vec(vec![sym("External"), ScVal::Address(verifier.clone()), bytes(&key.key_data())])
}

fn delegated(account: &ScAddress) -> ScVal {
    vec(vec![sym("Delegated"), ScVal::Address(account.clone())])
}

struct Fixture {
    s: Session,
    key: Ed25519Key,
    verifiers: BTreeMap<u32, ScAddress>,
}

fn fixture() -> Fixture {
    let mut s = session();
    let key = rehearse_alice(&s);
    let mut verifiers = BTreeMap::new();
    for kind in [ED25519, SECP256R1, WEBAUTHN] {
        let v = deploy(&mut s, &alice(), &key, "verifier", &format!("verifier-{kind}"), vec![ScVal::U32(kind)]);
        verifiers.insert(kind, v);
    }
    Fixture { s, key, verifiers }
}

impl Fixture {
    fn account(&mut self, salt: &str, signers: Vec<ScVal>, scheme: u32, valid_until: u32) -> ScAddress {
        let key = self.key.clone();
        deploy(&mut self.s, &alice(), &key, "smart_account", salt, vec![vec(signers), ScVal::U32(scheme), ScVal::U32(valid_until)])
    }

    fn signers(account: &ScAddress, signer: AuthSigner) -> BTreeMap<ScAddress, AuthSigner> {
        BTreeMap::from([(account.clone(), signer)])
    }

    fn try_prepare(&self, account: &ScAddress, signer: AuthSigner, expiration: Option<u32>) -> anyhow::Result<Prepared> {
        let auth = Self::signers(account, signer);
        self.s.prepare(&PrepareRequest {
            source: alice(),
            host_function: invoke(account, "guarded", vec![ScVal::U32(5)]),
            auth_signers: &auth,
            envelope_signers: std::slice::from_ref(&self.key),
            auth_expiration_ledgers: expiration,
            memo: soroban_sim_core::host::xdr::Memo::None,
            preconditions: soroban_sim_core::host::xdr::Preconditions::None,
        })
    }

    /// Prepare `account.guarded(5)` and submit it; returns the record.
    fn call(&mut self, label: &str, account: &ScAddress, signer: AuthSigner) -> TxRecord {
        let p = self.try_prepare(account, signer, None).unwrap_or_else(|e| panic!("{label}: {e:#}"));
        self.s.submit(label, &p.envelope).unwrap()
    }
}

fn oz(digest: OzDigest, signers: Vec<OzSigner>) -> AuthSigner {
    AuthSigner::OzSmartAccount { rule_ids: vec![0], digest, signers }
}

fn addr_oz_v1() -> OzDigest {
    OzDigest::AddressBound { domain: ADDR_OZ_V1_DOMAIN.to_vec() }
}

#[test]
fn oz_external_ed25519_under_both_digest_schemes() {
    let mut f = fixture();
    let admin = ExternalKey::Ed25519(Ed25519Key::throwaway("admin"));
    let v = f.verifiers[&ED25519].clone();
    let plain = f.account("acct-rule-ids", vec![external(&v, &admin)], RULE_IDS, 0);
    let bound = f.account("acct-addr-oz-v1", vec![external(&v, &admin)], ADDR_OZ_V1, 0);
    let signer = |d: OzDigest| oz(d, vec![OzSigner::External { verifier: v.clone(), key: admin.clone() }]);

    let r = f.call("oz_rule_ids", &plain, signer(OzDigest::RuleIds));
    assert_eq!(r.status.summary(), "5", "{}", r.status.summary());
    let r = f.call("addr_oz_v1", &bound, signer(addr_oz_v1()));
    assert_eq!(r.status.summary(), "5", "{}", r.status.summary());

    // The digest mismatch netsim found on mainnet: an account that checks
    // addr_oz_v1 rejects an oz_rule_ids signature, and vice versa.
    let err = f.try_prepare(&bound, signer(OzDigest::RuleIds), None).unwrap_err();
    assert!(format!("{err:#}").contains("Auth"), "{err:#}");
    assert!(f.try_prepare(&plain, signer(addr_oz_v1()), None).is_err());
}

#[test]
fn oz_external_with_the_wrong_key_is_refused() {
    let mut f = fixture();
    let admin = ExternalKey::Ed25519(Ed25519Key::throwaway("admin"));
    let mallory = ExternalKey::Ed25519(Ed25519Key::throwaway("mallory"));
    let v = f.verifiers[&ED25519].clone();
    let acct = f.account("acct", vec![external(&v, &admin)], ADDR_OZ_V1, 0);
    // An unregistered signer: the account refuses it (UnknownSigner = 1).
    let err = f.try_prepare(&acct, oz(addr_oz_v1(), vec![OzSigner::External { verifier: v.clone(), key: mallory }]), None).unwrap_err();
    assert!(format!("{err:#}").contains("Auth"), "{err:#}");

    // A registered signer whose signature bytes come from another key: the
    // verifier traps. Built after prepare so the applier sees it too.
    let good = oz(addr_oz_v1(), vec![OzSigner::External { verifier: v.clone(), key: admin.clone() }]);
    let p = f.try_prepare(&acct, good, None).unwrap();
    let mut entry = p.auth[0].clone();
    let SorobanCredentials::Address(c) = &mut entry.credentials else { panic!() };
    let ScVal::Map(Some(payload)) = &mut c.signature else { panic!() };
    let mut entries = payload.0.to_vec();
    let ScVal::Map(Some(signers)) = &mut entries[1].val else { panic!() };
    let mut s = signers.0.to_vec();
    s[0].val = bytes(&[0u8; 64]);
    *signers = soroban_sim_core::host::xdr::ScMap(s.try_into().unwrap());
    *payload = soroban_sim_core::host::xdr::ScMap(entries.try_into().unwrap());
    let key = f.key.clone();
    let env = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        let mut ops = tx.operations.to_vec();
        let soroban_sim_core::host::xdr::OperationBody::InvokeHostFunction(op) = &mut ops[0].body else { panic!() };
        op.auth = vec![entry].try_into().unwrap();
        tx.operations = ops.try_into().unwrap();
    });
    let r = f.s.submit("forged smart-account signature", &env).unwrap();
    expect_failed(&r, "");
    assert!(!r.status.is_applied());
}

#[test]
fn oz_external_secp256r1_and_webauthn() {
    let mut f = fixture();
    let p256 = ExternalKey::Secp256r1(Secp256r1Key::throwaway("hsm"));
    let passkey = ExternalKey::WebAuthn { key: Secp256r1Key::throwaway("passkey"), credential_id: b"credential-1".to_vec() };
    let (vp, vw) = (f.verifiers[&SECP256R1].clone(), f.verifiers[&WEBAUTHN].clone());
    let a = f.account("acct-p256", vec![external(&vp, &p256)], ADDR_OZ_V1, 0);
    let b = f.account("acct-webauthn", vec![external(&vw, &passkey)], ADDR_OZ_V1, 0);
    let r = f.call("secp256r1", &a, oz(addr_oz_v1(), vec![OzSigner::External { verifier: vp, key: p256 }]));
    assert_eq!(r.status.summary(), "5", "{}", r.status.summary());
    let r = f.call("webauthn", &b, oz(addr_oz_v1(), vec![OzSigner::External { verifier: vw.clone(), key: passkey }]));
    assert_eq!(r.status.summary(), "5", "{}", r.status.summary());
    // Another passkey's assertion does not verify against the stored key.
    let other = ExternalKey::WebAuthn { key: Secp256r1Key::throwaway("other"), credential_id: b"credential-1".to_vec() };
    assert!(f.try_prepare(&b, oz(addr_oz_v1(), vec![OzSigner::External { verifier: vw, key: other }]), None).is_err());
}

#[test]
fn cap71_delegate_signs_until_the_rule_expires() {
    let mut f = fixture();
    let ci = Ed25519Key::throwaway("ci-delegate");
    f.s.fund(&ci.account_id(), 20 * XLM, "ci delegate").unwrap();
    let ci_addr = ScAddress::Account(ci.account_id());
    let valid_until = f.s.ledger_info().sequence_number + 40;
    let acct = f.account("acct-ci", vec![delegated(&ci_addr)], RULE_IDS, valid_until);
    let signer = || oz(OzDigest::RuleIds, vec![OzSigner::Delegated { account: ci.account_id(), keys: vec![ci.clone()] }]);

    let r = f.call("delegated publish", &acct, signer());
    assert_eq!(r.status.summary(), "5", "{}", r.status.summary());
    assert!(matches!(&f.s.log().last().unwrap().status, TxStatus::Applied { .. }));

    // The delegate's key must be one of the G account's signers.
    let wrong = oz(OzDigest::RuleIds, vec![OzSigner::Delegated { account: ci.account_id(), keys: vec![Ed25519Key::throwaway("mallory")] }]);
    assert!(f.try_prepare(&acct, wrong, None).is_err());

    // Signed while the rule is live, submitted after it expires: refused at
    // apply with OZ's UnvalidatedContext (3002), and the fee is still paid.
    let early = f.try_prepare(&acct, signer(), Some(200)).unwrap();
    f.s.warp_to_ledger(valid_until + 1).unwrap();
    let r = f.s.submit("after the rule expired", &early.envelope).unwrap();
    expect_failed(&r, "Auth");
    assert!(r.contract_errors().contains(&3002), "{:?}", r.contract_errors());
    assert!(r.fee_charged > 0);
}

#[test]
fn classic_address_bound_credentials() {
    let mut s = session();
    let key = rehearse_alice(&s);
    let carol = Ed25519Key::throwaway("carol");
    s.fund(&carol.account_id(), 100 * XLM, "carol").unwrap();
    apply(&mut s, "deploy native SAC", &carol.account_id(), std::slice::from_ref(&carol), native_sac());
    let transfer = invoke(&sac_id(), "transfer", vec![
        ScVal::Address(ScAddress::Account(alice())),
        ScVal::Address(ScAddress::Account(carol.account_id())),
        ScVal::I128(Int128Parts { hi: 0, lo: XLM as u64 }),
    ]);
    let signers = BTreeMap::from([(ScAddress::Account(alice()), AuthSigner::Classic { keys: vec![key], address_bound: true })]);
    let p = prepare(&s, &carol.account_id(), std::slice::from_ref(&carol), transfer, &signers);
    assert!(matches!(p.auth[0].credentials, SorobanCredentials::AddressV2(_)));
    let r = s.submit("AddressV2 transfer", &p.envelope).unwrap();
    assert!(r.status.is_applied(), "{}", r.status.summary());
}

#[test]
fn throwaway_keys_are_deterministic_and_distinct() {
    assert_eq!(Ed25519Key::throwaway("a").public_key(), Ed25519Key::throwaway("a").public_key());
    assert_ne!(Ed25519Key::throwaway("a").public_key(), Ed25519Key::throwaway("b").public_key());
    assert_eq!(Secp256r1Key::throwaway("a").public_key(), Secp256r1Key::throwaway("a").public_key());
    assert!(Ed25519Key::throwaway("a").strkey().starts_with('G'));
    assert_eq!(Secp256r1Key::throwaway("a").public_key()[0], 4, "uncompressed SEC1");
}
