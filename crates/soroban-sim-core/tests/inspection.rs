#![cfg(exact_apply)]
mod common;
use common::*;
use soroban_sim_core::{host::xdr::*, session::*};
use std::collections::BTreeMap;

fn vec(values: Vec<ScVal>) -> ScVal {
    ScVal::Vec(Some(ScVec(values.try_into().unwrap())))
}
fn sym(value: &str) -> ScVal {
    ScVal::Symbol(ScSymbol(value.try_into().unwrap()))
}
fn signature(accept: bool, delegates: &[ScAddress]) -> ScVal {
    vec(vec![
        ScVal::Bool(accept),
        vec(delegates.iter().cloned().map(ScVal::Address).collect()),
    ])
}
fn custom(delegates: Vec<DelegateSigner>) -> AuthSigner {
    let addresses: Vec<_> = delegates.iter().map(|d| d.address.clone()).collect();
    AuthSigner::Custom {
        signer: CustomSigner::new(move |_| Ok(signature(true, &addresses))),
        delegates,
    }
}
fn traced(s: &mut Session, envelope: &TransactionEnvelope) -> TxRecord {
    s.submit_with(
        "inspect",
        envelope,
        &SubmitOptions {
            trace: true,
            ..Default::default()
        },
    )
    .unwrap()
}
fn context(address: &ScAddress, name: &str, args: Vec<ScVal>) -> ScVal {
    vec(vec![
        sym("Contract"),
        ScVal::Map(Some(ScMap(
            vec![
                ScMapEntry {
                    key: sym("args"),
                    val: vec(args),
                },
                ScMapEntry {
                    key: sym("contract"),
                    val: ScVal::Address(address.clone()),
                },
                ScMapEntry {
                    key: sym("fn_name"),
                    val: sym(name),
                },
            ]
            .try_into()
            .unwrap(),
        ))),
    ])
}
fn setup() -> (Session, Ed25519Key, ScAddress, ScAddress, ScAddress) {
    let mut options = SessionOptions::new(PASSPHRASE);
    options.enable_diagnostics = false;
    let mut s = Session::new(Box::new(base(vec![])), fork_point(), options).unwrap();
    let key = rehearse_alice(&s);
    let root = deploy(&mut s, &alice(), &key, "inspection", "root", vec![]);
    let child = deploy(&mut s, &alice(), &key, "inspection", "child", vec![]);
    let account = deploy(&mut s, &alice(), &key, "inspection", "account", vec![]);
    (s, key, root, child, account)
}
fn prepare_nested(
    s: &Session,
    key: &Ed25519Key,
    root: &ScAddress,
    child: &ScAddress,
    account: &ScAddress,
    kind: CredentialKind,
    delegates: Vec<DelegateSigner>,
) -> Prepared {
    let auth = BTreeMap::from([(account.clone(), custom(delegates))]);
    s.prepare_with(
        &PrepareRequest {
            source: alice(),
            host_function: invoke(
                root,
                "outer",
                vec![
                    ScVal::Address(account.clone()),
                    ScVal::Address(child.clone()),
                    ScVal::U32(9),
                ],
            ),
            auth_signers: &auth,
            envelope_signers: std::slice::from_ref(key),
            auth_expiration_ledgers: None,
            memo: Memo::None,
            preconditions: Preconditions::None,
        },
        &PrepareOptions {
            credential_kind: Some(kind),
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn auth_inputs_are_exact_for_each_credential_kind_and_nested_delegates() {
    for kind in [
        CredentialKind::Address,
        CredentialKind::AddressV2,
        CredentialKind::AddressWithDelegates,
    ] {
        let (mut s, key, root, child, account) = setup();
        let delegate = (kind == CredentialKind::AddressWithDelegates)
            .then(|| deploy(&mut s, &alice(), &key, "inspection", "delegate", vec![]));
        let delegates = delegate
            .iter()
            .map(|address| DelegateSigner {
                address: address.clone(),
                signer: CustomSigner::new(|_| Ok(signature(true, &[]))),
                delegates: vec![],
            })
            .collect();
        let p = prepare_nested(&s, &key, &root, &child, &account, kind, delegates);
        assert_eq!(p.auth.len(), 1);
        let (creds, bound) = match &p.auth[0].credentials {
            SorobanCredentials::Address(c) => (c, None),
            SorobanCredentials::AddressV2(c) => (c, Some(&c.address)),
            SorobanCredentials::AddressWithDelegates(c) => {
                (&c.address_credentials, Some(&c.address_credentials.address))
            }
            _ => panic!(),
        };
        let payload = auth_payload(s.network_id(), creds, &p.auth[0], bound).unwrap();
        let expected = vec![
            context(
                &root,
                "outer",
                vec![
                    ScVal::Address(account.clone()),
                    ScVal::Address(child.clone()),
                    ScVal::U32(9),
                ],
            ),
            context(
                &child,
                "inner",
                vec![ScVal::Address(account.clone()), ScVal::U32(9)],
            ),
        ];
        let checkpoint = s.checkpoint();
        let plain = s.submit("plain", &p.envelope).unwrap();
        assert!(plain.status.is_applied(), "{:?}", plain.status);
        assert!(plain.trace.is_none());
        s.restore(&checkpoint);
        let record = traced(&mut s, &p.envelope);
        assert!(record.status.is_applied(), "{:?}", record.status);
        assert_eq!(
            (
                plain.cpu_insns,
                plain.mem_bytes,
                plain.fee_charged,
                plain.success_hash
            ),
            (
                record.cpu_insns,
                record.mem_bytes,
                record.fee_charged,
                record.success_hash
            )
        );
        let trace = record.trace.unwrap();
        assert!(trace.complete);
        assert!(trace.auth_failures.is_empty());
        assert_eq!(
            trace.check_auth.len(),
            if delegate.is_some() { 2 } else { 1 }
        );
        for call in &trace.check_auth {
            assert_eq!(call.payload_hash, payload);
            assert_eq!(call.contexts, expected);
            assert_eq!(call.credential_kind, Some(kind));
        }
        assert_eq!(trace.check_auth[0].signature, creds.signature);
        if let Some(delegate) = delegate {
            assert_eq!(trace.check_auth[1].address, delegate);
            assert_eq!(trace.check_auth[1].delegate_of, Some(account));
        }
    }
}

fn edit_auth(
    p: &Prepared,
    key: &Ed25519Key,
    change: impl FnOnce(&mut SorobanAuthorizationEntry),
) -> TransactionEnvelope {
    resign(&p.envelope, std::slice::from_ref(key), |tx| {
        let mut ops = tx.operations.to_vec();
        let OperationBody::InvokeHostFunction(op) = &mut ops[0].body else {
            panic!()
        };
        let mut auth = op.auth.to_vec();
        change(&mut auth[0]);
        op.auth = auth.try_into().unwrap();
        tx.operations = ops.try_into().unwrap();
    })
}
#[test]
fn authorization_refusals_have_distinct_kinds_without_diagnostics() {
    let (mut s, key, root, child, account) = setup();
    let p = prepare_nested(
        &s,
        &key,
        &root,
        &child,
        &account,
        CredentialKind::AddressV2,
        vec![],
    );
    let cp = s.checkpoint();
    let variants = [
        (
            edit_auth(&p, &key, |e| {
                let SorobanAuthorizedFunction::ContractFn(f) = &mut e.root_invocation.function
                else {
                    panic!()
                };
                let mut args = f.args.to_vec();
                args[2] = ScVal::U32(10);
                f.args = args.try_into().unwrap();
            }),
            AuthFailureKind::ArgsMismatch,
        ),
        (
            edit_auth(&p, &key, |e| {
                e.root_invocation.sub_invocations = Default::default()
            }),
            AuthFailureKind::NodeMissing,
        ),
        (
            edit_auth(&p, &key, |e| {
                let SorobanCredentials::AddressV2(c) = &mut e.credentials else {
                    panic!()
                };
                c.signature_expiration_ledger = s.ledger_info().sequence_number - 1;
            }),
            AuthFailureKind::SignatureExpired,
        ),
        (
            edit_auth(&p, &key, |e| {
                let SorobanCredentials::AddressV2(c) = &mut e.credentials else {
                    panic!()
                };
                c.signature = signature(false, &[]);
            }),
            AuthFailureKind::CheckAuthRefused(Some(ScError::Contract(17))),
        ),
    ];
    for (envelope, kind) in variants {
        s.restore(&cp);
        let record = traced(&mut s, &envelope);
        assert!(
            matches!(record.status, TxStatus::Failed { .. }),
            "{:?}",
            record.status
        );
        let trace = record.trace.unwrap();
        assert!(trace.complete);
        assert!(
            trace.auth_failures.iter().any(|f| f.kind == kind),
            "expected {kind:?}: {:?}",
            trace.auth_failures
        );
    }
    s.restore(&cp);
    assert!(traced(&mut s, &p.envelope).status.is_applied());
    let replay = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        tx.seq_num.0 += 1
    });
    let record = traced(&mut s, &replay);
    assert!(record
        .trace
        .unwrap()
        .auth_failures
        .iter()
        .any(|f| f.kind == AuthFailureKind::NonceReused));
}
#[test]
fn prepare_supports_non_root_authorization() {
    let (mut s, key, root, child, account) = setup();
    let auth = BTreeMap::from([(account.clone(), custom(vec![]))]);
    let request = PrepareRequest {
        source: alice(),
        host_function: invoke(
            &root,
            "nonroot",
            vec![
                ScVal::Address(account),
                ScVal::Address(child.clone()),
                ScVal::U32(9),
            ],
        ),
        auth_signers: &auth,
        envelope_signers: std::slice::from_ref(&key),
        auth_expiration_ledgers: None,
        memo: Memo::None,
        preconditions: Preconditions::None,
    };
    assert!(s.prepare(&request).is_err());
    let p = s
        .prepare_with(
            &request,
            &PrepareOptions {
                allow_non_root_auth: true,
                ..Default::default()
            },
        )
        .unwrap();
    let SorobanAuthorizedFunction::ContractFn(function) = &p.auth[0].root_invocation.function
    else {
        panic!()
    };
    assert_eq!(function.contract_address, child);
    assert!(traced(&mut s, &p.envelope).status.is_applied());
}
#[test]
fn storage_traces_distinguish_durability_and_propagate_rollback() {
    let (mut s, key, root, child, _) = setup();
    let p = prepare(
        &s,
        &alice(),
        std::slice::from_ref(&key),
        invoke(
            &root,
            "rollback",
            vec![
                ScVal::Address(child.clone()),
                ScVal::U32(7),
                ScVal::Bool(true),
                ScVal::Bool(true),
            ],
        ),
        &no_auth(),
    );
    let data_key = |address: &ScAddress| {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: address.clone(),
            key: ScVal::U32(7),
            durability: ContractDataDurability::Persistent,
        })
    };
    let cp = s.checkpoint();
    let record = traced(&mut s, &p.envelope);
    assert!(s.ledger().get(&data_key(&root)).unwrap().is_some());
    assert!(s.ledger().get(&data_key(&child)).unwrap().is_none());
    assert!(record.status.is_applied(), "{:?}", record.status);
    let trace = record.trace.unwrap();
    assert!(trace.complete);
    let writes: Vec<_> = trace
        .storage
        .iter()
        .filter(|a| a.operation == "put")
        .collect();
    for durability in [
        Durability::Temporary,
        Durability::Persistent,
        Durability::Instance,
    ] {
        assert!(writes.iter().any(|a| a.address.as_ref() == Some(&root)
            && a.key == ScVal::U32(7)
            && a.durability == durability
            && a.disposition == Disposition::Committed));
    }
    assert!(writes
        .iter()
        .any(|a| a.address.as_ref() == Some(&child) && a.disposition == Disposition::RolledBack));
    for op in ["get", "has", "delete", "extend_ttl"] {
        assert!(trace.storage.iter().any(|a| a.operation == op), "{op}");
    }
    let non_refundable = record.fees.non_refundable;
    s.restore(&cp);
    let no_rent = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        let TransactionExt::V1(data) = &mut tx.ext else {
            panic!()
        };
        data.resource_fee = non_refundable;
    });
    let failed = traced(&mut s, &no_rent);
    assert!(
        matches!(
            failed.status,
            TxStatus::Failed {
                code: OpFailure::InsufficientRefundableFee,
                ..
            }
        ),
        "{:?}",
        failed.status
    );
    assert!(failed
        .trace
        .unwrap()
        .storage
        .iter()
        .all(|a| a.disposition == Disposition::RolledBack));
    assert!(s.ledger().get(&data_key(&root)).unwrap().is_none());
    s.restore(&cp);
    let uncaught = resign(&p.envelope, std::slice::from_ref(&key), |tx| {
        let mut ops = tx.operations.to_vec();
        let OperationBody::InvokeHostFunction(op) = &mut ops[0].body else {
            panic!()
        };
        let HostFunction::InvokeContract(f) = &mut op.host_function else {
            panic!()
        };
        let mut args = f.args.to_vec();
        args[2] = ScVal::Bool(false);
        f.args = args.try_into().unwrap();
        tx.operations = ops.try_into().unwrap();
    });
    let record = traced(&mut s, &uncaught);
    assert!(matches!(record.status, TxStatus::Failed { .. }));
    let trace = record.trace.unwrap();
    assert!(s.ledger().get(&data_key(&root)).unwrap().is_none());
    assert!(s.ledger().get(&data_key(&child)).unwrap().is_none());
    assert!(!trace.storage.is_empty());
    assert!(trace
        .storage
        .iter()
        .all(|a| a.disposition == Disposition::RolledBack));
}
