//! The session plan file (YAML or JSON). See `docs/session.md` for the
//! reference; `examples/perch-nido-bootstrap.yaml` is a full plan.
//!
//! Strings anywhere in a plan may use `${name}` interpolation: plan `vars`,
//! values saved by earlier steps, `key.NAME` (a key's G address),
//! `key.NAME.pk` (its public key, hex), `ledger` and `time` (the ledger the
//! next transaction applies in and its close time) and `network.passphrase`.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `mainnet`, `testnet`, or `{ rpc, passphrase }`. The CLI's `--fork` wins.
    #[serde(default)]
    pub network: Option<NetworkSpec>,
    #[serde(default)]
    pub options: PlanOptions,
    /// Throwaway keys by name: `ed25519`, `secp256r1`, `webauthn`, or
    /// `{ ed25519: label }` and so on. The label (default: the name) seeds the key.
    #[serde(default)]
    pub keys: BTreeMap<String, KeySpec>,
    /// Named values. They may reference each other in any order.
    #[serde(default)]
    pub vars: BTreeMap<String, ValueExpr>,
    /// Auth-entry signers by address, for every step.
    #[serde(default)]
    pub signers: BTreeMap<String, SignerSpec>,
    /// Overrides applied before the first step.
    #[serde(default)]
    pub setup: Vec<OverrideSpec>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum NetworkSpec {
    Named(String),
    Custom { rpc: String, passphrase: String },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanOptions {
    pub seconds_per_ledger: Option<u64>,
    pub auth_expiration_ledgers: Option<u32>,
    /// Where `upload` steps look for Wasm by hash, relative to the plan.
    pub wasm_dir: Option<String>,
    /// A network to fetch Wasm bytes from by hash (read-only) when no local
    /// file has them.
    pub bytes_source: Option<NetworkSpec>,
    /// Default envelope source for transaction steps.
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum KeySpec {
    Kind(String),
    Labeled {
        #[serde(default)]
        ed25519: Option<String>,
        #[serde(default)]
        secp256r1: Option<String>,
        #[serde(default)]
        webauthn: Option<String>,
        /// Hex; appended to a WebAuthn key's public key in its `key_data`.
        #[serde(default)]
        credential_id: Option<String>,
    },
}

/// A value: text (interpolated), a number, or a computed value.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ValueExpr {
    Text(String),
    Number(i64),
    Computed(Computed),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Computed {
    /// The id the host derives for `deployer` and a 32-byte hex `salt`.
    ContractId { deployer: Box<ValueExpr>, salt: Box<ValueExpr> },
    /// `sha256(utf8 text)`, as hex.
    Sha256(Box<ValueExpr>),
    /// The ledger the next transaction applies in, plus N.
    LedgerPlus(u32),
    /// The rendered result of a read-only call at evaluation time.
    View(CallSpec),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallSpec {
    pub contract: String,
    #[serde(rename = "fn")]
    pub function: String,
    /// ScVal JSON (`{ string: … }`, `{ address: … }`, `{ u32: … }`,
    /// `{ vec: [...] }`, `void`, …), plus `{ bytes_utf8: text }` and
    /// `{ bytes_sha256: text }`.
    #[serde(default)]
    pub args: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SignerSpec {
    /// A G account's ed25519 signers (`[key, …]` or `{ keys, address_bound }`).
    Classic(ClassicSpec),
    /// An OpenZeppelin-style smart account.
    Oz(OzSpec),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ClassicSpec {
    Keys(Vec<String>),
    Full {
        keys: Vec<String>,
        #[serde(default)]
        address_bound: bool,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OzSpec {
    /// `oz_rule_ids` or `addr_oz_v1`.
    pub digest: String,
    /// The `addr_oz_v1` domain (default `perch/account/auth-addr-v1`).
    #[serde(default)]
    pub domain: Option<String>,
    pub rule: RuleSpec,
    pub signers: Vec<OzSignerSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RuleSpec {
    Id(u32),
    Ids(Vec<u32>),
    /// Looked up by `get_context_rule(id)` on the account at signing time:
    /// rule ids move when an account's rules change.
    Named { name: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OzSignerSpec {
    External { verifier: String, key: String },
    /// CAP-71: a G account's keys sign in the entry's delegate list.
    Delegated { account: String, keys: Vec<String> },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverrideSpec {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub substitute_master_key: Option<SubstituteMaster>,
    #[serde(default)]
    pub substitute_signer: Option<SubstituteSigner>,
    #[serde(default)]
    pub fund: Option<Fund>,
    /// A `LedgerEntry` (base64 XDR) written as is.
    #[serde(default)]
    pub set_entry: Option<SetEntry>,
    /// A `LedgerKey` (base64 XDR) removed.
    #[serde(default)]
    pub remove_entry: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubstituteMaster {
    pub account: String,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubstituteSigner {
    pub account: String,
    /// The real signer's G address.
    pub signer: String,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fund {
    pub account: String,
    #[serde(default)]
    pub xlm: Option<ValueExpr>,
    #[serde(default)]
    pub stroops: Option<ValueExpr>,
}

/// One `assert` check: `value` (interpolated) must equal `equals`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub value: ValueExpr,
    pub equals: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetEntry {
    pub xdr: String,
    #[serde(default)]
    pub live_until: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default)]
    pub name: Option<String>,

    // Exactly one action.
    #[serde(default)]
    pub invoke: Option<CallSpec>,
    #[serde(default)]
    pub upload: Option<UploadSpec>,
    #[serde(default)]
    pub deploy: Option<DeploySpec>,
    /// `native`: deploy the native asset's Stellar Asset Contract.
    #[serde(default)]
    pub deploy_sac: Option<String>,
    /// A signed `TransactionEnvelope` (base64 XDR), submitted as is.
    #[serde(default)]
    pub envelope: Option<String>,
    /// Classic operations, each a one-operation transaction.
    #[serde(default)]
    pub create_account: Option<CreateAccountSpec>,
    #[serde(default)]
    pub payment: Option<PaymentSpec>,
    #[serde(default)]
    pub set_options: Option<SetOptionsSpec>,
    #[serde(default)]
    pub change_trust: Option<ChangeTrustSpec>,
    /// A classic transaction of several operations.
    #[serde(default)]
    pub operations: Option<Vec<OperationSpec>>,
    /// Transaction steps submitted for the same ledger: one transaction per
    /// source account and the per-ledger limits apply, and what does not fit
    /// waits for the next ledger.
    #[serde(default)]
    pub ledger: Option<Vec<Step>>,
    #[serde(default)]
    pub advance: Option<AdvanceSpec>,
    #[serde(default)]
    pub view: Option<CallSpec>,
    #[serde(default)]
    pub set: Option<BTreeMap<String, ValueExpr>>,
    #[serde(default, rename = "override")]
    pub override_: Option<OverrideSpec>,
    /// Checks on values, such as derived addresses against known ones.
    #[serde(default)]
    pub assert: Option<Vec<Check>>,

    // Transaction options.
    #[serde(default)]
    pub source: Option<String>,
    /// Envelope signers (key names). Default: the classic signers registered
    /// for the source, else the key whose G address is the source.
    #[serde(default)]
    pub sign_with: Option<Vec<String>>,
    /// Auth-entry signers for this step, over the plan's `signers`.
    #[serde(default)]
    pub auth: BTreeMap<String, SignerSpec>,
    #[serde(default)]
    pub auth_expiration_ledgers: Option<u32>,
    /// Prepare and sign, but do not submit (use with `save_envelope`).
    #[serde(default)]
    pub hold: bool,
    /// Save the step's signed envelope (base64 XDR) under this name.
    #[serde(default)]
    pub save_envelope: Option<String>,
    /// Wrap the transaction in a fee bump: another account pays its fee.
    #[serde(default)]
    pub fee_bump: Option<FeeBumpSpec>,
    /// For `view`: run against a fresh fork, without the session's writes
    /// and overrides.
    #[serde(default)]
    pub fresh_fork: bool,

    // Results.
    /// Save the return value (rendered) under this name.
    #[serde(default)]
    pub save: Option<String>,
    #[serde(default)]
    pub expect: Option<Expect>,
}

/// A fee bump around a transaction step: `source` pays the fee (and gets
/// any refund) instead of the transaction's own source.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeeBumpSpec {
    /// The fee source (a G address, or `${key.NAME}`).
    pub source: String,
    /// The outer fee in stroops. Default: the inner transaction's inclusion
    /// fee rate over one more operation (at least the base fee each), plus
    /// its resource fee.
    #[serde(default)]
    pub fee: Option<ValueExpr>,
    /// Who signs the fee bump (key names). Default: the classic signers
    /// registered for the fee source, else the key whose account it is.
    #[serde(default)]
    pub sign_with: Option<Vec<String>>,
}

/// `CreateAccount`: a new account funded from the source.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAccountSpec {
    /// The new account (a G address, or `${key.NAME}`).
    pub destination: String,
    /// XLM, as a number or a decimal string with up to 7 places.
    pub starting_balance: ValueExpr,
}

/// `Payment`: XLM or an issued asset.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentSpec {
    pub destination: String,
    /// `native` (the default) or `CODE:ISSUER`.
    #[serde(default)]
    pub asset: Option<String>,
    /// In units of the asset, up to 7 decimal places.
    pub amount: ValueExpr,
}

/// `SetOptions`. Weights and thresholds are 0 to 255.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetOptionsSpec {
    #[serde(default)]
    pub master_weight: Option<u32>,
    #[serde(default)]
    pub low_threshold: Option<u32>,
    #[serde(default)]
    pub med_threshold: Option<u32>,
    #[serde(default)]
    pub high_threshold: Option<u32>,
    #[serde(default)]
    pub home_domain: Option<String>,
    /// Flag names (`auth_required`, `auth_revocable`, `auth_immutable`,
    /// `auth_clawback_enabled`) or the bits.
    #[serde(default)]
    pub set_flags: Option<FlagsSpec>,
    #[serde(default)]
    pub clear_flags: Option<FlagsSpec>,
    #[serde(default)]
    pub inflation_dest: Option<String>,
    /// Add, reweigh, or (weight 0) remove an ed25519 signer.
    #[serde(default)]
    pub signer: Option<SignerWeightSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FlagsSpec {
    Names(Vec<String>),
    Bits(u32),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignerWeightSpec {
    /// A plan key's name or a G address.
    pub key: String,
    pub weight: u32,
}

/// `ChangeTrust`: open, change or (limit 0) close a trustline.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeTrustSpec {
    /// `CODE:ISSUER`.
    pub asset: String,
    /// In units of the asset; default the maximum.
    #[serde(default)]
    pub limit: Option<ValueExpr>,
}

/// One operation of an `operations` step.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationSpec {
    /// The operation's source, if not the transaction's.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub create_account: Option<CreateAccountSpec>,
    #[serde(default)]
    pub payment: Option<PaymentSpec>,
    #[serde(default)]
    pub set_options: Option<SetOptionsSpec>,
    #[serde(default)]
    pub change_trust: Option<ChangeTrustSpec>,
}

impl OperationSpec {
    pub fn actions(&self) -> usize {
        [self.create_account.is_some(), self.payment.is_some(), self.set_options.is_some(), self.change_trust.is_some()].iter().filter(|b| **b).count()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadSpec {
    /// A Wasm file (relative to the plan).
    #[serde(default)]
    pub file: Option<String>,
    /// The Wasm hash (hex): checked against `file`, or used to find the bytes
    /// in the wasm dirs or the bytes source.
    #[serde(default)]
    pub hash: Option<String>,
    /// Skip the upload when the code is already on the ledger, live or
    /// archived (default true).
    #[serde(default)]
    pub skip_if_present: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploySpec {
    pub deployer: String,
    /// 32-byte hex, or a computed value such as `{ sha256: text }`.
    pub salt: ValueExpr,
    pub wasm_hash: String,
    #[serde(default)]
    pub args: Vec<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvanceSpec {
    #[serde(default)]
    pub ledgers: Option<u32>,
    #[serde(default)]
    pub seconds: Option<u64>,
    #[serde(default)]
    pub to_ledger: Option<ValueExpr>,
    #[serde(default)]
    pub to_time: Option<ValueExpr>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The return value, as ScVal JSON.
    #[serde(default)]
    pub value: Option<Value>,
    /// The rendered return value (an address, a string's text, a number, hex bytes).
    #[serde(default)]
    pub equals: Option<String>,
    /// The step must NOT apply. A string must appear in the outcome
    /// (`txBAD_AUTH`, `3002`, …).
    #[serde(default)]
    pub fails: Option<Fails>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Fails {
    Any(bool),
    Containing(String),
}

impl Plan {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        // YAML goes through a JSON value so enums read as `{ variant: … }`
        // maps in both formats (serde_yaml alone wants `!variant` tags).
        let value: Value = match path.extension().and_then(|e| e.to_str()) {
            Some("json") => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            _ => serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        };
        let plan: Plan = serde_path_to_error::deserialize(value).with_context(|| format!("reading the plan in {}", path.display()))?;
        plan.check()?;
        Ok(plan)
    }

    /// Structural checks that need no network.
    pub fn check(&self) -> Result<()> {
        for (i, step) in self.steps.iter().enumerate() {
            step.check(&format!("step {} ({})", i + 1, step.label(i)))?;
            if let Some(nested) = &step.ledger {
                if nested.is_empty() {
                    bail!("step {} ({}): `ledger` needs at least one transaction step", i + 1, step.label(i));
                }
                for (j, inner) in nested.iter().enumerate() {
                    let at = format!("step {}.{} ({})", i + 1, j + 1, inner.label(j));
                    inner.check(&at)?;
                    if !inner.is_transaction() || inner.ledger.is_some() {
                        bail!("{at}: a `ledger` block holds transaction steps only, not {}", inner.kind());
                    }
                    if inner.hold {
                        bail!("{at}: `hold` has no meaning inside a `ledger` block");
                    }
                }
            }
        }
        for (name, k) in &self.keys {
            if let KeySpec::Kind(kind) = k {
                if !matches!(kind.as_str(), "ed25519" | "secp256r1" | "webauthn") {
                    bail!("key {name}: unknown kind {kind} (ed25519, secp256r1 or webauthn)");
                }
            }
        }
        Ok(())
    }
}

impl Step {
    fn check(&self, at: &str) -> Result<()> {
        let n = self.actions();
        if n != 1 {
            bail!("{at} must have exactly one action, has {n}");
        }
        if self.hold && self.save_envelope.is_none() {
            bail!("{at}: `hold` needs `save_envelope`");
        }
        if self.fee_bump.is_some() && !self.is_transaction() {
            bail!("{at}: `fee_bump` needs a transaction step");
        }
        if let Some(ops) = &self.operations {
            if ops.is_empty() {
                bail!("{at}: `operations` needs at least one operation");
            }
            for (k, op) in ops.iter().enumerate() {
                if op.actions() != 1 {
                    bail!("{at}: operation {} must be exactly one of create_account, payment, set_options, change_trust", k + 1);
                }
            }
        }
        Ok(())
    }

    fn actions(&self) -> usize {
        [
            self.invoke.is_some(),
            self.upload.is_some(),
            self.deploy.is_some(),
            self.deploy_sac.is_some(),
            self.envelope.is_some(),
            self.create_account.is_some(),
            self.payment.is_some(),
            self.set_options.is_some(),
            self.change_trust.is_some(),
            self.operations.is_some(),
            self.ledger.is_some(),
            self.advance.is_some(),
            self.view.is_some(),
            self.set.is_some(),
            self.override_.is_some(),
            self.assert.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }

    pub fn kind(&self) -> &'static str {
        if self.invoke.is_some() {
            "invoke"
        } else if self.upload.is_some() {
            "upload"
        } else if self.deploy.is_some() {
            "deploy"
        } else if self.deploy_sac.is_some() {
            "deploy_sac"
        } else if self.envelope.is_some() {
            "envelope"
        } else if self.create_account.is_some() {
            "create_account"
        } else if self.payment.is_some() {
            "payment"
        } else if self.set_options.is_some() {
            "set_options"
        } else if self.change_trust.is_some() {
            "change_trust"
        } else if self.operations.is_some() {
            "operations"
        } else if self.ledger.is_some() {
            "ledger"
        } else if self.advance.is_some() {
            "advance"
        } else if self.view.is_some() {
            "view"
        } else if self.set.is_some() {
            "set"
        } else if self.assert.is_some() {
            "assert"
        } else {
            "override"
        }
    }

    pub fn label(&self, index: usize) -> String {
        self.name.clone().unwrap_or_else(|| format!("{} #{}", self.kind(), index + 1))
    }

    pub fn is_transaction(&self) -> bool {
        matches!(
            self.kind(),
            "invoke" | "upload" | "deploy" | "deploy_sac" | "envelope" | "create_account" | "payment" | "set_options" | "change_trust" | "operations"
        )
    }

    /// A step of classic operations.
    pub fn is_classic(&self) -> bool {
        matches!(self.kind(), "create_account" | "payment" | "set_options" | "change_trust" | "operations")
    }
}

/// Replace every `${name}` in `text` using `lookup`.
pub fn interpolate(text: &str, lookup: &mut dyn FnMut(&str) -> Result<String>) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').with_context(|| format!("unclosed ${{ in {text:?}"))?;
        out.push_str(&lookup(after[..end].trim())?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}
