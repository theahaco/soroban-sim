//! The WIT `network-session` resource: a [`Facade`] over a [`DeferredBase`]
//! the caller fills in. Every method is one facade JSON-RPC call, so the
//! typed methods and `rpc` share one implementation with `soroban-sim serve`.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use soroban_env_host::xdr::{LedgerEntry, LedgerHeader, LedgerKey, Limits, ReadXdr, WriteXdr};
use soroban_sim_core::session::{ForkPoint, SessionOptions, StoredEntry};
use soroban_sim_facade::{open, DeferredBase, Facade, FacadeConfig, Opened, Reply as FacadeReply};
use std::cell::RefCell;
use std::rc::Rc;

use crate::exports::theahaco::soroban_sim::network_session::{
    BaseBatch, ForkConfig, GuestSession, LedgerOverride, Reply,
};

enum State {
    /// Not opened yet: opening reads the network's settings from the base.
    Pending(Box<Pending>),
    Open(Box<Facade>),
}

struct Pending {
    fork: ForkPoint,
    options: SessionOptions,
    config: FacadeConfig,
}

pub struct NetworkSession {
    base: Rc<DeferredBase>,
    state: RefCell<State>,
    /// The caller's base RPC counters, applied once the facade is open.
    counters: RefCell<(u32, u32)>,
}

impl GuestSession for NetworkSession {
    fn new(config: ForkConfig) -> Self {
        let mut options = SessionOptions::new(&config.network_passphrase);
        if config.seconds_per_ledger > 0 {
            options.seconds_per_ledger = config.seconds_per_ledger as u64;
        }
        let fork = ForkPoint {
            ledger_seq: config.fork_ledger,
            close_time: config.fork_close_time,
            protocol_version: config.protocol_version,
            base_reserve: config.base_reserve,
            base_fee: config.base_fee,
        };
        // A header that does not decode is dropped for one built from the
        // fork point, as for a snapshot base.
        let header = config.header_xdr.and_then(|b| LedgerHeader::from_xdr(b, Limits::none()).ok());
        let facade = FacadeConfig {
            network_name: config.network_name,
            passphrase: config.network_passphrase,
            base_description: config.base_description.clone(),
            header,
            allow_real_signers: config.allow_real_signers,
            served_by: config.served_by,
        };
        Self {
            base: DeferredBase::new(&config.base_description),
            state: RefCell::new(State::Pending(Box::new(Pending { fork, options, config: facade }))),
            counters: RefCell::new((0, 0)),
        }
    }

    fn provide(&self, batch: BaseBatch) -> Result<(), String> {
        let entries = batch
            .entries
            .into_iter()
            .map(|e| {
                Ok(StoredEntry {
                    entry: LedgerEntry::from_xdr(e.entry_xdr, Limits::none()).map_err(|e| format!("a base entry is not LedgerEntry XDR: {e}"))?,
                    live_until: e.live_until_ledger,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let absent = batch
            .absent_keys
            .into_iter()
            .map(|k| LedgerKey::from_xdr(k, Limits::none()).map_err(|e| format!("an absent key is not LedgerKey XDR: {e}")))
            .collect::<Result<Vec<_>, String>>()?;
        self.base.provide(batch.served_at_ledger, entries, absent).map_err(|e| format!("{e:#}"))
    }

    fn rpc(&self, body: String) -> Result<Reply, String> {
        self.answer(body.as_bytes())
    }

    fn submit(&self, envelope_xdr: Vec<u8>) -> Result<Reply, String> {
        self.call("sendTransaction", json!({ "transaction": B64.encode(envelope_xdr) }))
    }

    fn advance(&self, ledgers: u32, seconds: Option<u64>) -> Result<Reply, String> {
        self.call("fork_advanceLedgers", json!({ "ledgers": ledgers, "seconds": seconds }))
    }

    fn set_time(&self, time: u64) -> Result<Reply, String> {
        self.call("fork_setTime", json!({ "time": time }))
    }

    fn apply_override(&self, change: LedgerOverride) -> Result<Reply, String> {
        match change {
            LedgerOverride::SetBalance(o) => self.call("fork_setBalance", json!({ "account": o.account, "stroops": o.stroops, "label": o.label })),
            LedgerOverride::AddSigner(o) => self.call(
                "fork_addSigner",
                json!({ "account": o.account, "signer": o.signer, "replace": o.replace.unwrap_or_else(|| "master".into()), "label": o.label }),
            ),
            LedgerOverride::SetEntry(o) => self.call(
                "fork_setLedgerEntry",
                json!({ "entry": B64.encode(o.entry_xdr), "liveUntilLedgerSeq": o.live_until_ledger, "label": o.label }),
            ),
            LedgerOverride::RemoveEntry(o) => self.call("fork_removeLedgerEntry", json!({ "key": B64.encode(o.key_xdr), "label": o.label })),
        }
    }

    fn snapshot(&self, label: String) -> Result<Reply, String> {
        self.call("fork_snapshot", json!({ "label": label }))
    }

    fn revert(&self, id: u32) -> Result<Reply, String> {
        self.call("fork_revert", json!({ "id": id }))
    }

    fn allow_real_signers(&self, allow: bool) -> Result<Reply, String> {
        self.call("fork_allowRealSigners", json!({ "allow": allow }))
    }

    fn steps(&self) -> Result<Reply, String> {
        self.call("fork_steps", Value::Null)
    }

    fn debug_context(&self, step: u32) -> Result<Reply, String> {
        self.call("fork_debugContext", json!({ "step": step }))
    }

    fn report(&self) -> Result<Reply, String> {
        self.call("fork_report", Value::Null)
    }

    fn set_rpc_counters(&self, calls: u32, throttled_retries: u32) {
        *self.counters.borrow_mut() = (calls, throttled_retries);
        if let State::Open(f) = &mut *self.state.borrow_mut() {
            f.set_rpc_counters(calls, throttled_retries);
        }
    }
}

impl NetworkSession {
    fn call(&self, method: &str, params: Value) -> Result<Reply, String> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        self.answer(body.to_string().as_bytes())
    }

    /// Open the session if it is not yet, then answer `body`. Either step
    /// may need base entries; a call that does is undone.
    fn answer(&self, body: &[u8]) -> Result<Reply, String> {
        let mut state = self.state.borrow_mut();
        if let State::Pending(p) = &*state {
            match open(&self.base, p.fork.clone(), p.options.clone(), p.config.clone()).map_err(|e| format!("{e:#}"))? {
                Opened::Ready(mut facade) => {
                    let (calls, throttled) = *self.counters.borrow();
                    facade.set_rpc_counters(calls, throttled);
                    *state = State::Open(facade);
                }
                Opened::NeedsState(keys) => return needs_state(keys),
            }
        }
        let State::Open(facade) = &mut *state else { unreachable!("opened above") };
        match facade.answer(&self.base, body) {
            FacadeReply::Response(v) => Ok(Reply::Response(v.map(|v| v.to_string()))),
            FacadeReply::NeedsState(keys) => needs_state(keys),
        }
    }
}

fn needs_state(keys: Vec<LedgerKey>) -> Result<Reply, String> {
    let keys = keys.iter().map(|k| k.to_xdr(Limits::none()).map_err(|e| e.to_string())).collect::<Result<Vec<_>, _>>()?;
    Ok(Reply::NeedsState(keys))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::theahaco::soroban_sim::network_session::{BalanceOverride, BaseEntry};
    use soroban_env_host::xdr::{LedgerEntryData, ReadXdr};
    use soroban_sim_core::session::{account_key, host_protocol, Ed25519Key};

    const PASSPHRASE: &str = "soroban-sim component tests ; 2026";
    const FORK_LEDGER: u32 = 10_000_000;

    fn config() -> ForkConfig {
        ForkConfig {
            network_name: "test".into(),
            network_passphrase: PASSPHRASE.into(),
            base_description: "component test base".into(),
            fork_ledger: FORK_LEDGER,
            fork_close_time: 1_790_000_000,
            protocol_version: host_protocol(),
            base_reserve: 5_000_000,
            base_fee: 100,
            header_xdr: None,
            seconds_per_ledger: 0,
            allow_real_signers: false,
            served_by: "component tests".into(),
        }
    }

    /// Real testnet settings, keyed by LedgerKey XDR.
    fn settings() -> Vec<(Vec<u8>, Vec<u8>)> {
        let json: serde_json::Value = serde_json::from_str(include_str!("../fixtures/parity/native-sac-transfer.json")).unwrap();
        json["snapshot"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| {
                let b64 = |k: &str| B64.decode(e[k].as_str().unwrap()).unwrap();
                let entry = LedgerEntry::from_xdr(b64("entryXdr"), Limits::none()).unwrap();
                matches!(entry.data, LedgerEntryData::ConfigSetting(_)).then(|| (b64("keyXdr"), b64("entryXdr")))
            })
            .collect()
    }

    /// Answer every needs-state from `settings` (anything else is absent).
    fn answer(s: &NetworkSession, mut call: impl FnMut() -> Result<Reply, String>) -> (Value, usize) {
        let base = settings();
        for rounds in 0.. {
            match call().unwrap() {
                Reply::Response(Some(r)) => return (serde_json::from_str(&r).unwrap(), rounds),
                Reply::Response(None) => panic!("no response"),
                Reply::NeedsState(keys) => {
                    let (present, absent): (Vec<_>, Vec<_>) = keys.into_iter().partition(|k| base.iter().any(|(bk, _)| bk == k));
                    let entries = present
                        .iter()
                        .map(|k| BaseEntry { entry_xdr: base.iter().find(|(bk, _)| bk == k).unwrap().1.clone(), live_until_ledger: None })
                        .collect();
                    s.provide(BaseBatch { served_at_ledger: FORK_LEDGER, entries, absent_keys: absent }).unwrap();
                }
            }
        }
        unreachable!()
    }

    #[test]
    fn opening_takes_one_round_for_every_network_setting() {
        let s = NetworkSession::new(config());
        let first = s.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"getNetwork"}"#.into()).unwrap();
        let Reply::NeedsState(keys) = first else { panic!("expected needs-state, got {first:?}") };
        assert_eq!(keys.len(), 13);
        let (r, rounds) = answer(&s, || s.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"getNetwork"}"#.into()));
        assert_eq!(rounds, 1);
        assert_eq!(r["result"]["passphrase"], PASSPHRASE);
    }

    #[test]
    fn typed_methods_are_facade_calls() {
        let s = NetworkSession::new(config());
        let carol = Ed25519Key::throwaway("carol");
        let (r, _) = answer(&s, || {
            s.apply_override(LedgerOverride::SetBalance(BalanceOverride { account: carol.strkey(), stroops: 250 * 10_000_000, label: "carol".into() }))
        });
        assert_eq!(r["result"]["balance"], "2500000000", "{r}");
        let (snap, _) = answer(&s, || s.snapshot("before advancing".into()));
        let (clock, _) = answer(&s, || s.advance(10, None));
        assert_eq!(clock["result"]["next"]["ledger"], FORK_LEDGER + 11);
        let (clock, _) = answer(&s, || s.set_time(1_790_001_000));
        assert_eq!(clock["result"]["next"]["closeTime"], 1_790_001_000u64);
        let (reverted, _) = answer(&s, || s.revert(snap["result"]["id"].as_u64().unwrap() as u32));
        assert_eq!(reverted["result"]["reverted"], 2);
        assert_eq!(reverted["result"]["next"]["ledger"], FORK_LEDGER + 1);

        let (steps, _) = answer(&s, || s.steps());
        let kinds: Vec<&str> = steps["result"]["steps"].as_array().unwrap().iter().map(|s| s["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["override", "snapshot", "advance", "advance", "revert"]);
        let override_change = &steps["result"]["steps"][0]["changes"][0];
        assert_eq!(override_change["change"], "created");
        let key = LedgerKey::from_xdr_base64(override_change["key"].as_str().unwrap(), Limits::none()).unwrap();
        assert_eq!(key, account_key(&carol.account_id()));

        let (bad, _) = answer(&s, || s.submit(vec![1, 2, 3]));
        assert_eq!(bad["error"]["code"], -32602);
        let (bad, _) = answer(&s, || s.debug_context(1));
        assert_eq!(bad["error"]["code"], -32602, "an override is not a traceable transaction");
        let (report, _) = answer(&s, || s.report());
        assert!(report["result"]["markdown"].as_str().unwrap().contains("## Overrides (1)"));
    }

    #[test]
    fn a_protocol_mismatch_is_an_error_not_a_needs_state() {
        let mut c = config();
        c.protocol_version = host_protocol() - 1;
        let s = NetworkSession::new(c);
        let err = (0..3)
            .find_map(|_| match s.rpc(r#"{"jsonrpc":"2.0","id":1,"method":"fork_info"}"#.into()) {
                Err(e) => Some(e),
                Ok(Reply::NeedsState(keys)) => {
                    let base = settings();
                    let (present, absent): (Vec<_>, Vec<_>) = keys.into_iter().partition(|k| base.iter().any(|(bk, _)| bk == k));
                    let entries = present.iter().filter_map(|k| base.iter().find(|(bk, _)| bk == k)).map(|(_, e)| BaseEntry { entry_xdr: e.clone(), live_until_ledger: None }).collect();
                    s.provide(BaseBatch { served_at_ledger: FORK_LEDGER, entries, absent_keys: absent }).unwrap();
                    None
                }
                Ok(other) => panic!("expected an error, got {other:?}"),
            })
            .unwrap();
        assert!(err.contains("exact protocol parity"), "{err}");
    }
}
