//! Throwaway keys and the signer adapters that turn a recorded authorization
//! entry into a signed one.
//!
//! A rehearsal never holds a real key. Every key here is derived from a label
//! (`sha256("soroban-sim throwaway:" || label)`), and the session substitutes
//! it for the real signer on the forked ledger.
//!
//! Adapters:
//!
//! - [`AuthSigner::Classic`]: a G account's ed25519 signers, verified by the
//!   host against the account's weights and medium threshold. With
//!   `address_bound` the entry becomes CAP-71 `AddressV2`, whose payload binds
//!   the address.
//! - [`AuthSigner::OzSmartAccount`]: an OpenZeppelin-style smart account's
//!   `AuthPayload` (`context_rule_ids` plus a `Signer -> bytes` map) under one
//!   of two digest schemes ([`OzDigest`]). `External(verifier, key)` signers
//!   sign with ed25519, raw secp256r1 or a WebAuthn passkey assertion;
//!   `Delegated(G)` signers ride in a CAP-71 `AddressWithDelegates` entry.

use anyhow::{anyhow, bail, Result};
use ed25519_dalek::Signer as _;
use p256::ecdsa::signature::hazmat::PrehashSigner as _;
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    AccountId, BytesM, ContractId, DecoratedSignature, Hash, HashIdPreimage,
    HashIdPreimageSorobanAuthorization, HashIdPreimageSorobanAuthorizationWithAddress, Limits,
    PublicKey, ScAddress, ScBytes, ScMap, ScMapEntry, ScSymbol, ScVal, ScVec, SignatureHint,
    SorobanAddressCredentials, SorobanAddressCredentialsWithDelegates, SorobanAuthorizationEntry,
    SorobanCredentials, SorobanDelegateSignature, Uint256, WriteXdr,
};

/// `perch/account/auth-addr-v1`: the domain of the `addr_oz_v1` digest.
pub const ADDR_OZ_V1_DOMAIN: &[u8] = b"perch/account/auth-addr-v1";

const THROWAWAY_DOMAIN: &str = "soroban-sim throwaway:";

/// A throwaway ed25519 key.
#[derive(Clone)]
pub struct Ed25519Key {
    label: String,
    key: ed25519_dalek::SigningKey,
}

impl Ed25519Key {
    /// The key for `label`: `sha256("soroban-sim throwaway:" || label)`.
    pub fn throwaway(label: &str) -> Self {
        let seed: [u8; 32] = Sha256::digest(format!("{THROWAWAY_DOMAIN}{label}")).into();
        Self { label: label.to_string(), key: ed25519_dalek::SigningKey::from_bytes(&seed) }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn account_id(&self) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(self.public_key())))
    }

    /// The `G…` address.
    pub fn strkey(&self) -> String {
        stellar_strkey::ed25519::PublicKey(self.public_key()).to_string()
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.key.sign(msg).to_bytes()
    }

    /// An envelope signature over a transaction hash.
    pub fn sign_decorated(&self, tx_hash: &[u8; 32]) -> DecoratedSignature {
        let pk = self.public_key();
        DecoratedSignature {
            hint: SignatureHint([pk[28], pk[29], pk[30], pk[31]]),
            signature: self.sign(tx_hash).to_vec().try_into().expect("64 bytes"),
        }
    }
}

impl std::fmt::Debug for Ed25519Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ed25519Key({}, {})", self.label, self.strkey())
    }
}

/// A throwaway secp256r1 (P-256) key, for raw secp256r1 and WebAuthn signers.
#[derive(Clone)]
pub struct Secp256r1Key {
    label: String,
    key: p256::ecdsa::SigningKey,
}

/// One WebAuthn assertion: the three fields of OZ's `WebAuthnSigData`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebAuthnAssertion {
    pub authenticator_data: Vec<u8>,
    pub client_data: Vec<u8>,
    /// `r || s`, low-s normalized (the host refuses high-s signatures).
    pub signature: [u8; 64],
}

/// The relying party throwaway passkeys name. Verifiers check the challenge
/// and flags, not the origin or the rp id hash.
pub const WEBAUTHN_RP_ID: &str = "soroban-sim.invalid";
const WEBAUTHN_ORIGIN: &str = "https://soroban-sim.invalid";
/// User present and user verified, no backup bits.
const WEBAUTHN_FLAGS: u8 = 0x05;

impl Secp256r1Key {
    /// The key for `label`, derived like [`Ed25519Key::throwaway`] (a counter
    /// is appended in the vanishingly rare case the hash is not a valid scalar).
    pub fn throwaway(label: &str) -> Self {
        for counter in 0u32.. {
            let seed: [u8; 32] = Sha256::digest(format!("{THROWAWAY_DOMAIN}secp256r1:{label}:{counter}")).into();
            if let Ok(key) = p256::ecdsa::SigningKey::from_bytes(&seed.into()) {
                return Self { label: label.to_string(), key };
            }
        }
        unreachable!("a valid P-256 scalar is found within a few tries")
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// The 65-byte uncompressed SEC1 public key.
    pub fn public_key(&self) -> [u8; 65] {
        let point = self.key.verifying_key().to_encoded_point(false);
        point.as_bytes().try_into().expect("uncompressed point")
    }

    /// ECDSA over `sha256(msg)`, low-s normalized.
    pub fn sign_sha256(&self, msg: &[u8]) -> [u8; 64] {
        self.sign_prehash(&Sha256::digest(msg).into())
    }

    fn sign_prehash(&self, prehash: &[u8; 32]) -> [u8; 64] {
        let sig: p256::ecdsa::Signature = self.key.sign_prehash(prehash).expect("P-256 signing");
        let sig = sig.normalize_s().unwrap_or(sig);
        sig.to_bytes().into()
    }

    /// A WebAuthn `webauthn.get` assertion whose challenge is `challenge`, as a
    /// user-verifying authenticator produces it.
    pub fn webauthn_assert(&self, challenge: &[u8; 32]) -> WebAuthnAssertion {
        let client_data = format!(
            r#"{{"type":"webauthn.get","challenge":"{}","origin":"{WEBAUTHN_ORIGIN}","crossOrigin":false}}"#,
            base64url(challenge)
        )
        .into_bytes();
        let mut authenticator_data = Sha256::digest(WEBAUTHN_RP_ID.as_bytes()).to_vec();
        authenticator_data.push(WEBAUTHN_FLAGS);
        authenticator_data.extend_from_slice(&1u32.to_be_bytes());
        let mut message = authenticator_data.clone();
        message.extend_from_slice(&Sha256::digest(&client_data));
        WebAuthnAssertion { signature: self.sign_sha256(&message), authenticator_data, client_data }
    }
}

impl std::fmt::Debug for Secp256r1Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secp256r1Key({})", self.label)
    }
}

/// How an OZ smart account turns the host's signature payload into the
/// digest its `External` signers sign.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OzDigest {
    /// `oz_rule_ids`: `sha256(payload || ScVec[u32 rule ids] XDR)` (OZ
    /// `stellar-accounts` `do_check_auth`).
    RuleIds,
    /// `addr_oz_v1`: the payload is first bound to the account's contract id,
    /// `sha256(domain || id || payload)`, then hashed with the rule ids as
    /// above. The usual domain is [`ADDR_OZ_V1_DOMAIN`].
    AddressBound { domain: Vec<u8> },
}

/// The key half of an OZ `Signer::External(verifier, key)`.
#[derive(Clone, Debug)]
pub enum ExternalKey {
    /// 32-byte key; signs the digest.
    Ed25519(Ed25519Key),
    /// 65-byte key; ECDSA-SHA256 over the digest.
    Secp256r1(Secp256r1Key),
    /// 65-byte key followed by `credential_id`; a WebAuthn assertion whose
    /// challenge is the digest, encoded as `WebAuthnSigData` XDR.
    WebAuthn { key: Secp256r1Key, credential_id: Vec<u8> },
}

impl ExternalKey {
    /// The `key_data` the account stores for this signer.
    pub fn key_data(&self) -> Vec<u8> {
        match self {
            Self::Ed25519(k) => k.public_key().to_vec(),
            Self::Secp256r1(k) => k.public_key().to_vec(),
            Self::WebAuthn { key, credential_id } => [key.public_key().as_slice(), credential_id].concat(),
        }
    }

    fn sign(&self, digest: &[u8; 32]) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Ed25519(k) => k.sign(digest).to_vec(),
            Self::Secp256r1(k) => k.sign_sha256(digest).to_vec(),
            Self::WebAuthn { key, .. } => webauthn_sig_data(&key.webauthn_assert(digest))?,
        })
    }
}

/// One signer an OZ smart account's `AuthPayload` names.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum OzSigner {
    External { verifier: ScAddress, key: ExternalKey },
    /// CAP-71: the account names `Delegated(account)` with empty signature
    /// bytes, and the G account's own signers sign the address-bound payload
    /// in the entry's delegate list.
    Delegated { account: AccountId, keys: Vec<Ed25519Key> },
}

/// A signature adapter for an arbitrary custom account. It receives the exact
/// host preimage hash (the root account's hash for every delegate).
type SignCallback = dyn Fn(&[u8; 32]) -> Result<ScVal>;
#[derive(Clone)]
pub struct CustomSigner(std::rc::Rc<SignCallback>);
impl CustomSigner {
    pub fn new(sign: impl Fn(&[u8; 32]) -> Result<ScVal> + 'static) -> Self { Self(std::rc::Rc::new(sign)) }
}
impl std::fmt::Debug for CustomSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("CustomSigner(..)") }
}
#[derive(Clone, Debug)]
pub struct DelegateSigner {
    pub address: ScAddress,
    pub signer: CustomSigner,
    pub delegates: Vec<DelegateSigner>,
}
fn sign_delegates(nodes: &[DelegateSigner], payload: &[u8; 32]) -> Result<Vec<SorobanDelegateSignature>> {
    let mut result = nodes.iter().map(|node| Ok(SorobanDelegateSignature {
        address: node.address.clone(), signature: (node.signer.0)(payload)?,
        nested_delegates: sign_delegates(&node.delegates, payload)?.try_into()?,
    })).collect::<Result<Vec<_>>>()?;
    result.sort_by(|a, b| a.address.cmp(&b.address));
    if result.windows(2).any(|w| w[0].address == w[1].address) { bail!("duplicate delegate address"); }
    Ok(result)
}

/// How the authorization entry for one address gets signed.
#[derive(Clone, Debug)]
pub enum AuthSigner {
    Custom { signer: CustomSigner, delegates: Vec<DelegateSigner> },
    Classic { keys: Vec<Ed25519Key>, address_bound: bool },
    OzSmartAccount { rule_ids: Vec<u32>, digest: OzDigest, signers: Vec<OzSigner> },
}

impl AuthSigner {
    pub fn classic(keys: Vec<Ed25519Key>) -> Self {
        Self::Classic { keys, address_bound: false }
    }
}

/// `sha256` of the authorization preimage the host builds for an entry.
/// With `address` it is CAP-71's `SorobanAuthorizationWithAddress`.
pub fn auth_payload(
    network_id: [u8; 32],
    creds: &SorobanAddressCredentials,
    entry: &SorobanAuthorizationEntry,
    address: Option<&ScAddress>,
) -> Result<[u8; 32]> {
    let preimage = match address {
        Some(address) => HashIdPreimage::SorobanAuthorizationWithAddress(HashIdPreimageSorobanAuthorizationWithAddress {
            network_id: Hash(network_id),
            nonce: creds.nonce,
            signature_expiration_ledger: creds.signature_expiration_ledger,
            address: address.clone(),
            invocation: entry.root_invocation.clone(),
        }),
        None => HashIdPreimage::SorobanAuthorization(HashIdPreimageSorobanAuthorization {
            network_id: Hash(network_id),
            nonce: creds.nonce,
            signature_expiration_ledger: creds.signature_expiration_ledger,
            invocation: entry.root_invocation.clone(),
        }),
    };
    Ok(Sha256::digest(preimage.to_xdr(Limits::none())?).into())
}

/// The digest an OZ `External` signer signs.
pub fn oz_digest(scheme: &OzDigest, account: &ScAddress, payload: &[u8; 32], rule_ids: &[u32]) -> Result<[u8; 32]> {
    let base: [u8; 32] = match scheme {
        OzDigest::RuleIds => *payload,
        OzDigest::AddressBound { domain } => {
            let ScAddress::Contract(ContractId(Hash(id))) = account else {
                bail!("the addr_oz_v1 digest binds a contract address, not {account}");
            };
            Sha256::digest([domain.as_slice(), id, payload].concat()).into()
        }
    };
    let ids = vec_val(rule_ids.iter().map(|i| ScVal::U32(*i)).collect())?;
    Ok(Sha256::digest([base.as_slice(), &ids.to_xdr(Limits::none())?].concat()).into())
}

/// Sign `entry` in place. Only entries with (plain, unsigned) `Address`
/// credentials are signed: the shape recording-mode simulation produces.
pub fn sign_auth_entry(entry: &mut SorobanAuthorizationEntry, signer: &AuthSigner, network_id: [u8; 32], expiration_ledger: u32) -> Result<()> {
    let mut creds = match &entry.credentials {
        SorobanCredentials::Address(c) | SorobanCredentials::AddressV2(c) => c.clone(),
        SorobanCredentials::SourceAccount => return Ok(()),
        SorobanCredentials::AddressWithDelegates(_) => bail!("entry is already signed with delegates"),
    };
    creds.signature_expiration_ledger = expiration_ledger;
    let recorded_v2 = matches!(entry.credentials, SorobanCredentials::AddressV2(_));
    match signer {
        AuthSigner::Custom { signer, delegates } => {
            let bound = (recorded_v2 || !delegates.is_empty()).then(|| creds.address.clone());
            let payload = auth_payload(network_id, &creds, entry, bound.as_ref())?;
            creds.signature = (signer.0)(&payload)?;
            entry.credentials = if !delegates.is_empty() {
                SorobanCredentials::AddressWithDelegates(SorobanAddressCredentialsWithDelegates { address_credentials: creds, delegates: sign_delegates(delegates, &payload)?.try_into()? })
            } else if recorded_v2 { SorobanCredentials::AddressV2(creds) } else { SorobanCredentials::Address(creds) };
        }
        AuthSigner::Classic { keys, address_bound } => {
            let address_bound = *address_bound || recorded_v2;
            let bound = address_bound.then(|| creds.address.clone());
            let payload = auth_payload(network_id, &creds, entry, bound.as_ref())?;
            creds.signature = classic_signature(keys, &payload)?;
            entry.credentials = if address_bound { SorobanCredentials::AddressV2(creds) } else { SorobanCredentials::Address(creds) };
        }
        AuthSigner::OzSmartAccount { rule_ids, digest, signers } => {
            if signers.is_empty() {
                bail!("an OZ smart-account signer needs at least one signer");
            }
            // An entry with delegates uses the address-bound preimage, both for
            // the account's own `__check_auth` and for every delegate.
            let with_delegates = signers.iter().any(|s| matches!(s, OzSigner::Delegated { .. }));
            let bound = (with_delegates || recorded_v2).then(|| creds.address.clone());
            let payload = auth_payload(network_id, &creds, entry, bound.as_ref())?;
            let oz = oz_digest(digest, &creds.address, &payload, rule_ids)?;
            let mut map = vec![];
            let mut delegates = vec![];
            for s in signers {
                match s {
                    OzSigner::External { verifier, key } => {
                        let name = vec_val(vec![sym("External")?, ScVal::Address(verifier.clone()), bytes_val(&key.key_data())?])?;
                        map.push((name, bytes_val(&key.sign(&oz)?)?));
                    }
                    OzSigner::Delegated { account, keys } => {
                        let address = ScAddress::Account(account.clone());
                        map.push((vec_val(vec![sym("Delegated")?, ScVal::Address(address.clone())])?, bytes_val(&[])?));
                        delegates.push(SorobanDelegateSignature {
                            address,
                            signature: classic_signature(keys, &payload)?,
                            nested_delegates: Default::default(),
                        });
                    }
                }
            }
            creds.signature = map_val(vec![
                (sym("context_rule_ids")?, vec_val(rule_ids.iter().map(|i| ScVal::U32(*i)).collect())?),
                (sym("signers")?, map_val(map)?),
            ])?;
            entry.credentials = if delegates.is_empty() {
                if recorded_v2 { SorobanCredentials::AddressV2(creds) } else { SorobanCredentials::Address(creds) }
            } else {
                // The host requires delegates sorted by address, without duplicates.
                delegates.sort_by(|a, b| a.address.cmp(&b.address));
                delegates.dedup_by(|a, b| a.address == b.address);
                SorobanCredentials::AddressWithDelegates(SorobanAddressCredentialsWithDelegates {
                    address_credentials: creds,
                    delegates: delegates.try_into()?,
                })
            };
        }
    }
    Ok(())
}

/// A G account's signature: `Vec<AccountEd25519Signature>` sorted by key.
fn classic_signature(keys: &[Ed25519Key], payload: &[u8; 32]) -> Result<ScVal> {
    if keys.is_empty() {
        bail!("a classic signer needs at least one key");
    }
    let mut keys: Vec<&Ed25519Key> = keys.iter().collect();
    keys.sort_by_key(|k| k.public_key());
    keys.dedup_by_key(|k| k.public_key());
    vec_val(
        keys.into_iter()
            .map(|k| map_val(vec![(sym("public_key")?, bytes_val(&k.public_key())?), (sym("signature")?, bytes_val(&k.sign(payload))?)]))
            .collect::<Result<_>>()?,
    )
}

/// OZ's `WebAuthnSigData { signature, authenticator_data, client_data }` XDR.
pub fn webauthn_sig_data(a: &WebAuthnAssertion) -> Result<Vec<u8>> {
    Ok(map_val(vec![
        (sym("authenticator_data")?, bytes_val(&a.authenticator_data)?),
        (sym("client_data")?, bytes_val(&a.client_data)?),
        (sym("signature")?, bytes_val(&a.signature)?),
    ])?
    .to_xdr(Limits::none())?)
}

/// Unpadded base64url.
pub fn base64url(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(input.len() * 4 / 3 + 2);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn sym(s: &str) -> Result<ScVal> {
    Ok(ScVal::Symbol(ScSymbol(s.try_into().map_err(|_| anyhow!("bad symbol {s}"))?)))
}

fn bytes_val(b: &[u8]) -> Result<ScVal> {
    Ok(ScVal::Bytes(ScBytes(BytesM::try_from(b.to_vec())?)))
}

fn vec_val(v: Vec<ScVal>) -> Result<ScVal> {
    Ok(ScVal::Vec(Some(ScVec(v.try_into()?))))
}

/// An `ScMap` with its keys in the host's order.
fn map_val(mut entries: Vec<(ScVal, ScVal)>) -> Result<ScVal> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(ScVal::Map(Some(ScMap(entries.into_iter().map(|(key, val)| ScMapEntry { key, val }).collect::<Vec<_>>().try_into()?))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_rfc4648_vectors() {
        for (input, want) in [("", ""), ("f", "Zg"), ("fo", "Zm8"), ("foo", "Zm9v"), ("foob", "Zm9vYg"), ("fooba", "Zm9vYmE"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64url(input.as_bytes()), want);
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64url(&[7; 32]).len(), 43);
    }

    #[test]
    fn oz_digests_follow_their_definitions() {
        let payload = [9u8; 32];
        let id = [3u8; 32];
        let account = ScAddress::Contract(ContractId(Hash(id)));
        // ScVal::Vec([U32(1), U32(2)]) XDR, written out.
        let ids_xdr: Vec<u8> = [&[0, 0, 0, 16, 0, 0, 0, 1, 0, 0, 0, 2][..], &[0, 0, 0, 3, 0, 0, 0, 1], &[0, 0, 0, 3, 0, 0, 0, 2]].concat();
        let plain: [u8; 32] = Sha256::digest([payload.as_slice(), &ids_xdr].concat()).into();
        assert_eq!(oz_digest(&OzDigest::RuleIds, &account, &payload, &[1, 2]).unwrap(), plain);
        let bound: [u8; 32] = Sha256::digest([ADDR_OZ_V1_DOMAIN, &id, &payload].concat()).into();
        let want: [u8; 32] = Sha256::digest([bound.as_slice(), &ids_xdr].concat()).into();
        let scheme = OzDigest::AddressBound { domain: ADDR_OZ_V1_DOMAIN.to_vec() };
        assert_eq!(oz_digest(&scheme, &account, &payload, &[1, 2]).unwrap(), want);
        let g = ScAddress::Account(Ed25519Key::throwaway("g").account_id());
        assert!(oz_digest(&scheme, &g, &payload, &[1]).is_err(), "addr_oz_v1 binds a contract id");
    }

    #[test]
    fn webauthn_assertions_sign_what_verifiers_check() {
        use p256::ecdsa::signature::hazmat::PrehashVerifier;
        let key = Secp256r1Key::throwaway("passkey");
        let a = key.webauthn_assert(&[5; 32]);
        let client = String::from_utf8(a.client_data.clone()).unwrap();
        assert!(client.contains(r#""type":"webauthn.get""#));
        assert!(client.contains(&format!(r#""challenge":"{}""#, base64url(&[5; 32]))));
        assert_eq!(a.authenticator_data[32] & 0x05, 0x05, "user present and verified");
        let message = [a.authenticator_data.as_slice(), &Sha256::digest(&a.client_data)].concat();
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&key.public_key()).unwrap();
        let sig = p256::ecdsa::Signature::from_slice(&a.signature).unwrap();
        assert!(sig.normalize_s().is_none(), "low-s");
        vk.verify_prehash(&Sha256::digest(&message), &sig).unwrap();
    }
}
