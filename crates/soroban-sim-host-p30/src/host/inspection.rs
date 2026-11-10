//! Read-only observations for differential tests. Conversion uses the shadow
//! budget; failures are reported to the observer and never change execution.
use crate::{
    budget::AsBudget,
    xdr::{ScAddress, ScError, ScSymbol, ScVal, SorobanAuthorizedFunction},
    AddressObject, Host, HostError, StorageType, TraceEvent, Val,
};

#[derive(Clone, Debug)]
pub enum Event {
    LedgerStorage {
        operation: &'static str,
        key: crate::xdr::LedgerKey,
        succeeded: bool,
    },
    Storage {
        operation: &'static str,
        address: ScAddress,
        key: ScVal,
        durability: StorageType,
        succeeded: bool,
    },
    CheckAuth {
        address: ScAddress,
        payload: ScVal,
        signature: ScVal,
        contexts: ScVal,
    },
    AuthFailure {
        address: ScAddress,
        kind: &'static str,
        error: Option<ScError>,
        function: Option<SorobanAuthorizedFunction>,
    },
    Unavailable,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // Transient observation, never retained by the host.
pub enum FrameEvent {
    Enter {
        address: Option<ScAddress>,
        function: Option<ScSymbol>,
        check_auth: Option<Event>,
    },
    Exit {
        error: Option<ScError>,
    },
}

impl TraceEvent<'_> {
    pub fn frame_event(&self, host: &Host) -> Result<Option<FrameEvent>, HostError> {
        use crate::host::Frame;
        let mut result = Err(HostError::from(crate::Error::from_type_and_code(
            crate::xdr::ScErrorType::Budget,
            crate::xdr::ScErrorCode::ExceededLimit,
        )));
        host.as_budget().with_shadow_mode(|| {
            result = (|| {
                Ok(match self {
                    TraceEvent::PushCtx(ctx) => {
                        let (id, name, args) = match &ctx.frame {
                            Frame::ContractVM {
                                vm, fn_name, args, ..
                            } => (
                                Some(vm.contract_id.clone()),
                                Some(*fn_name),
                                args.as_slice(),
                            ),
                            Frame::StellarAssetContract(id, name, args, _) => {
                                (Some(id.clone()), Some(*name), args.as_slice())
                            }
                            Frame::HostFunction(_) => (None, None, &[][..]),
                            #[cfg(any(test, feature = "testutils"))]
                            Frame::TestContract(tc) => {
                                (Some(tc.id.clone()), Some(tc.func), tc.args.as_slice())
                            }
                        };
                        let function = name
                            .map(|n| host.from_host_val(n.into()))
                            .transpose()?
                            .and_then(|v| {
                                if let ScVal::Symbol(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            });
                        let address = id.map(ScAddress::Contract);
                        let check_auth = if function
                            .as_ref()
                            .is_some_and(|f| f.0.as_slice() == b"__check_auth")
                            && args.len() == 3
                        {
                            address
                                .clone()
                                .map(|address| {
                                    Ok::<_, HostError>(Event::CheckAuth {
                                        address,
                                        payload: host.from_host_val(args[0])?,
                                        signature: host.from_host_val(args[1])?,
                                        contexts: host.from_host_val(args[2])?,
                                    })
                                })
                                .transpose()?
                        } else {
                            None
                        };
                        Some(FrameEvent::Enter {
                            address,
                            function,
                            check_auth,
                        })
                    }
                    TraceEvent::PopCtx(_, result) => Some(FrameEvent::Exit {
                        error: result
                            .as_ref()
                            .err()
                            .and_then(|e| ScError::try_from(e.error).ok()),
                    }),
                    _ => None,
                })
            })();
            Ok(())
        });
        result
    }
}

impl Host {
    pub(crate) fn observe(&self, make: impl FnOnce() -> Result<Event, HostError>) {
        if !self
            .try_borrow_trace_hook()
            .map(|h| h.is_some())
            .unwrap_or(false)
        {
            return;
        }
        let mut event = Event::Unavailable;
        self.as_budget().with_shadow_mode(|| {
            event = make()?;
            Ok(())
        });
        // Inspection hooks cannot fail execution or spend its metered budget.
        let _ = self.call_any_lifecycle_hook(TraceEvent::Inspection(&event));
    }

    pub(crate) fn observe_storage(
        &self,
        operation: &'static str,
        key: Val,
        durability: StorageType,
        succeeded: bool,
    ) {
        self.observe(|| {
            Ok(Event::Storage {
                operation,
                address: ScAddress::Contract(self.get_current_contract_id_internal()?),
                key: self.from_host_val(key)?,
                durability,
                succeeded,
            })
        });
    }

    pub(crate) fn observe_auth_failure(&self, address: AddressObject, kind: &'static str) {
        self.observe(|| {
            Ok(Event::AuthFailure {
                address: self.scaddress_from_address(address)?,
                kind,
                error: None,
                function: None,
            })
        });
    }
}
