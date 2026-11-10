//! Typed observations from the enforcing host, independent of diagnostic text.
use soroban_env_host::xdr::{
    LedgerKey, ScAddress, ScError, ScSymbol, ScVal,
    SorobanAuthorizedFunction,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    Address,
    AddressV2,
    AddressWithDelegates,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    Temporary,
    Persistent,
    Instance,
    Classic,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Committed,
    RolledBack,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthFailureKind {
    ArgsMismatch,
    NodeMissing,
    NonceReused,
    SignatureExpired,
    SignatureExpirationTooLate,
    CheckAuthRefused(Option<ScError>),
}
#[derive(Clone, Debug)]
pub struct AuthFailure {
    pub frame: Option<usize>,
    pub address: ScAddress,
    pub kind: AuthFailureKind,
    pub function: Option<SorobanAuthorizedFunction>,
}
#[derive(Clone, Debug)]
pub struct CheckAuthCall {
    pub frame: Option<usize>,
    pub address: ScAddress,
    pub credential_kind: Option<CredentialKind>,
    /// Root credential address when this is a delegate call.
    pub delegate_of: Option<ScAddress>,
    pub payload_hash: [u8; 32],
    pub signature: ScVal,
    /// The exact context values passed by the host, in preorder.
    pub contexts: Vec<ScVal>,
}
#[derive(Clone, Debug)]
pub struct InvocationFrame {
    pub id: usize,
    pub parent: Option<usize>,
    pub address: Option<ScAddress>,
    pub function: Option<ScSymbol>,
    pub error: Option<ScError>,
    pub disposition: Disposition,
}
#[derive(Clone, Debug)]
pub struct StorageAccess {
    pub frame: Option<usize>,
    pub operation: String,
    pub address: Option<ScAddress>,
    /// Exact ledger key; None for individual instance-storage subkeys.
    pub ledger_key: Option<LedgerKey>,
    pub key: ScVal,
    pub durability: Durability,
    /// Whether this operation returned successfully, before any frame rollback.
    pub succeeded: bool,
    pub disposition: Disposition,
}
#[derive(Clone, Debug, Default)]
pub struct TransactionTrace {
    /// False if an observation exceeded the host's shadow budget.
    pub complete: bool,
    pub frames: Vec<InvocationFrame>,
    pub check_auth: Vec<CheckAuthCall>,
    /// Includes refusals caught by a contract, even when the transaction succeeds.
    pub auth_failures: Vec<AuthFailure>,
    pub storage: Vec<StorageAccess>,
}
impl TransactionTrace {
    pub(crate) fn finalize(&mut self, committed: bool) {
        for index in 0..self.frames.len() {
            let frame = &self.frames[index];
            let rolled_back = !committed
                || frame.error.is_some()
                || frame
                    .parent
                    .is_some_and(|p| self.frames[p].disposition == Disposition::RolledBack);
            self.frames[index].disposition = if rolled_back {
                Disposition::RolledBack
            } else {
                Disposition::Committed
            };
        }
        for access in &mut self.storage {
            access.disposition = if !committed || !access.succeeded {
                Disposition::RolledBack
            } else {
                access
                    .frame
                    .map(|f| self.frames[f].disposition)
                    .unwrap_or(Disposition::Committed)
            };
        }
    }
}

#[cfg(exact_apply)]
pub(crate) mod capture {
    use super::super::applier::host;
    use super::*;
    use soroban_env_host::xdr::ContractDataDurability;
    use host::{
        inspection::{Event, FrameEvent},
        TraceEvent, TraceHook,
    };
    use soroban_env_host::xdr::{
        SorobanAuthorizationEntry, SorobanCredentials, SorobanDelegateSignature,
    };
    use std::{cell::RefCell, rc::Rc};

    type Credential = (ScAddress, ScVal, CredentialKind, Option<ScAddress>);
    fn delegates(nodes: &[SorobanDelegateSignature], root: &ScAddress, out: &mut Vec<Credential>) {
        for node in nodes {
            out.push((
                node.address.clone(),
                node.signature.clone(),
                CredentialKind::AddressWithDelegates,
                Some(root.clone()),
            ));
            delegates(node.nested_delegates.as_slice(), root, out);
        }
    }
    pub(crate) fn hook(
        auth: &[SorobanAuthorizationEntry],
    ) -> (TraceHook, Rc<RefCell<TransactionTrace>>) {
        let mut credentials = vec![];
        for entry in auth {
            match &entry.credentials {
                SorobanCredentials::Address(c) => credentials.push((
                    c.address.clone(),
                    c.signature.clone(),
                    CredentialKind::Address,
                    None,
                )),
                SorobanCredentials::AddressV2(c) => credentials.push((
                    c.address.clone(),
                    c.signature.clone(),
                    CredentialKind::AddressV2,
                    None,
                )),
                SorobanCredentials::AddressWithDelegates(c) => {
                    credentials.push((
                        c.address_credentials.address.clone(),
                        c.address_credentials.signature.clone(),
                        CredentialKind::AddressWithDelegates,
                        None,
                    ));
                    delegates(
                        c.delegates.as_slice(),
                        &c.address_credentials.address,
                        &mut credentials,
                    );
                }
                _ => {}
            }
        }
        let trace = Rc::new(RefCell::new(TransactionTrace {
            complete: true,
            ..Default::default()
        }));
        let output = trace.clone();
        let stack = RefCell::new(Vec::<usize>::new());
        let hook = Rc::new(move |host: &host::Host, event: TraceEvent<'_>| {
            let mut trace = output.borrow_mut();
            let mut stack = stack.borrow_mut();
            let mut current = stack.last().copied();
            let mut observation = if let TraceEvent::Inspection(e) = &event {
                Some((*e).clone())
            } else {
                None
            };
            match event.frame_event(host) {
                Ok(Some(FrameEvent::Enter {
                    address,
                    function,
                    check_auth,
                })) => {
                    let id = trace.frames.len();
                    trace.frames.push(InvocationFrame {
                        id,
                        parent: current,
                        address,
                        function,
                        error: None,
                        disposition: Disposition::RolledBack,
                    });
                    stack.push(id);
                    current = Some(id);
                    observation = check_auth;
                }
                Ok(Some(FrameEvent::Exit { error })) => {
                    if let Some(id) = stack.pop() {
                        let frame = &mut trace.frames[id];
                        if frame
                            .function
                            .as_ref()
                            .is_some_and(|f| f.0.as_slice() == b"__check_auth")
                            && error.is_some()
                        {
                            if let Some(address) = &frame.address {
                                observation = Some(Event::AuthFailure {
                                    address: address.clone(),
                                    kind: "CheckAuthRefused",
                                    error: error.clone(),
                                    function: None,
                                });
                            }
                        }
                        frame.error = error;
                    } else {
                        trace.complete = false;
                    }
                }
                Err(_) => trace.complete = false,
                _ => {}
            }
            if let Some(event) = &observation {
                match event {
                    Event::Unavailable => trace.complete = false,
                    Event::LedgerStorage {
                        operation,
                        key,
                        succeeded,
                    } => {
                        let (address, value, durability) = match key {
                            LedgerKey::ContractData(data) => (
                                Some(data.contract.clone()),
                                data.key.clone(),
                                if data.key == ScVal::LedgerKeyContractInstance {
                                    Durability::Instance
                                } else if data.durability == ContractDataDurability::Temporary {
                                    Durability::Temporary
                                } else {
                                    Durability::Persistent
                                },
                            ),
                            LedgerKey::ContractCode(code) => (
                                None,
                                ScVal::Bytes(
                                    code.hash.0.to_vec().try_into().expect("32-byte hash"),
                                ),
                                Durability::Persistent,
                            ),
                            LedgerKey::Account(account) => (
                                Some(ScAddress::Account(account.account_id.clone())),
                                ScVal::Void,
                                Durability::Classic,
                            ),
                            _ => (None, ScVal::Void, Durability::Classic),
                        };
                        trace.storage.push(StorageAccess {
                            frame: current,
                            operation: (*operation).into(),
                            address,
                            key: value,
                            ledger_key: Some(key.clone()),
                            durability,
                            succeeded: *succeeded,
                            disposition: Disposition::RolledBack,
                        });
                    }
                    Event::Storage {
                        operation,
                        address,
                        key,
                        durability,
                        succeeded,
                    } => trace.storage.push(StorageAccess {
                        frame: current,
                        operation: match *operation {
                            "get_contract_data" => "get",
                            "has_contract_data" => "has",
                            "put_contract_data" => "put",
                            "del_contract_data" => "delete",
                            other => other,
                        }
                        .into(),
                        address: Some(address.clone()),
                        ledger_key: None,
                        key: key.clone(),
                        durability: match durability {
                            host::StorageType::Temporary => Durability::Temporary,
                            host::StorageType::Persistent => Durability::Persistent,
                            host::StorageType::Instance => Durability::Instance,
                        },
                        succeeded: *succeeded,
                        disposition: Disposition::RolledBack,
                    }),
                    Event::CheckAuth {
                        address,
                        payload,
                        signature,
                        contexts,
                    } => {
                        if let (ScVal::Bytes(payload), ScVal::Vec(Some(contexts))) =
                            (payload, contexts)
                        {
                            if let Ok(payload_hash) = payload.as_slice().try_into() {
                                let credential = credentials
                                    .iter()
                                    .find(|c| &c.0 == address && &c.1 == signature);
                                trace.check_auth.push(CheckAuthCall {
                                    frame: current,
                                    address: address.clone(),
                                    credential_kind: credential.map(|c| c.2),
                                    delegate_of: credential.and_then(|c| c.3.clone()),
                                    payload_hash,
                                    signature: signature.clone(),
                                    contexts: contexts.to_vec(),
                                });
                            } else {
                                trace.complete = false;
                            }
                        } else {
                            trace.complete = false;
                        }
                    }
                    Event::AuthFailure {
                        address,
                        kind,
                        error,
                        function,
                    } => {
                        let kind = match *kind {
                            "ArgsMismatch" => AuthFailureKind::ArgsMismatch,
                            "NodeMissing" => AuthFailureKind::NodeMissing,
                            "NonceReused" => AuthFailureKind::NonceReused,
                            "SignatureExpired" => AuthFailureKind::SignatureExpired,
                            "SignatureExpirationTooLate" => {
                                AuthFailureKind::SignatureExpirationTooLate
                            }
                            "CheckAuthRefused" => AuthFailureKind::CheckAuthRefused(error.clone()),
                            _ => {
                                trace.complete = false;
                                return Ok(());
                            }
                        };
                        trace.auth_failures.push(AuthFailure {
                            frame: current,
                            address: address.clone(),
                            kind,
                            function: function.clone(),
                        });
                    }
                }
            }
            Ok(())
        });
        (hook, trace)
    }
}
