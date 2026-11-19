// XDR, StrKey and hashing for <soroban-sign-dialog>: a reader for the subset
// of Stellar XDR a signing review needs, and the authorization-tree
// comparison the preview uses. No DOM, no dependencies; the element and the
// preview engine both import it.

export class XdrError extends Error {}

const MAX_DEPTH = 64;
const td = new TextDecoder('utf-8', { fatal: false });

class Reader {
  constructor(bytes) {
    this.b = bytes;
    this.v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    this.o = 0;
  }
  need(n) {
    if (n < 0 || this.o + n > this.b.length) throw new XdrError('truncated XDR');
  }
  u32() { this.need(4); const x = this.v.getUint32(this.o); this.o += 4; return x; }
  i32() { this.need(4); const x = this.v.getInt32(this.o); this.o += 4; return x; }
  u64() { this.need(8); const x = this.v.getBigUint64(this.o); this.o += 8; return x; }
  i64() { this.need(8); const x = this.v.getBigInt64(this.o); this.o += 8; return x; }
  bool() {
    const x = this.u32();
    if (x > 1) throw new XdrError('bad bool');
    return x === 1;
  }
  fixed(n) {
    const padded = (n + 3) & ~3;
    this.need(padded);
    const x = this.b.subarray(this.o, this.o + n);
    for (let i = this.o + n; i < this.o + padded; i++) if (this.b[i]) throw new XdrError('non-zero padding');
    this.o += padded;
    return x;
  }
  var(max = Infinity) {
    const n = this.u32();
    if (n > max) throw new XdrError(`length ${n} over limit ${max}`);
    return this.fixed(n);
  }
  str(max) { return td.decode(this.var(max)); }
  arr(f, max = Infinity) {
    const n = this.u32();
    if (n > max || n > this.b.length - this.o) throw new XdrError('bad array length');
    const out = new Array(n);
    for (let i = 0; i < n; i++) out[i] = f();
    return out;
  }
  opt(f) { return this.bool() ? f() : null; }
  span(f) {
    const start = this.o;
    const value = f();
    return [value, this.b.subarray(start, this.o)];
  }
  done() {
    if (this.o !== this.b.length) throw new XdrError(`${this.b.length - this.o} trailing bytes`);
  }
}

export const fromB64 = (s) => {
  const bin = atob(String(s).trim());
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
};
export const toB64 = (bytes) => {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
};
export const hex = (bytes) => Array.from(bytes, (x) => x.toString(16).padStart(2, '0')).join('');
const bytesOf = (x) => (x instanceof Uint8Array ? x : fromB64(x));

function readPublicKey(r) {
  const t = r.u32();
  if (t !== 0) throw new XdrError(`public key type ${t}`);
  return r.fixed(32);
}

function readScAddress(r) {
  const t = r.u32();
  switch (t) {
    case 0: return address('account', readPublicKey(r));
    case 1: return address('contract', r.fixed(32));
    case 2: { const id = r.u64(); return address('muxed', r.fixed(32), id); }
    case 3: {
      const ct = r.u32();
      if (ct !== 0) throw new XdrError(`claimable balance id type ${ct}`);
      return address('claimable', r.fixed(32));
    }
    case 4: return address('pool', r.fixed(32));
    case 5: {
      // CAP-84 muxed contract (protocol 30): shown as the contract plus its
      // mux id, so labels for the contract still apply.
      const id = r.u64();
      const a = address('contract', r.fixed(32));
      return { ...a, kind: 'muxed-contract', id, str: a.str, muxId: id };
    }
    default: throw new XdrError(`unsupported address type ${t}`);
  }
}

function address(kind, key, id) {
  return { t: 'address', kind, key, id, str: strkeyFor(kind, key, id) };
}

function readScVal(r, depth = 0) {
  if (depth > MAX_DEPTH) throw new XdrError('ScVal nested too deeply');
  const t = r.u32();
  const next = () => readScVal(r, depth + 1);
  switch (t) {
    case 0: return { t: 'bool', v: r.bool() };
    case 1: return { t: 'void' };
    case 2: {
      const type = r.u32();
      return { t: 'error', type, code: r.u32() };
    }
    case 3: return { t: 'u32', v: r.u32() };
    case 4: return { t: 'i32', v: r.i32() };
    case 5: return { t: 'u64', v: r.u64() };
    case 6: return { t: 'i64', v: r.i64() };
    case 7: return { t: 'timepoint', v: r.u64() };
    case 8: return { t: 'duration', v: r.u64() };
    case 9: { const hi = r.u64(); return { t: 'u128', v: (hi << 64n) | r.u64() }; }
    case 10: { const hi = r.i64(); return { t: 'i128', v: (hi << 64n) | r.u64() }; }
    case 11: case 12: {
      const hh = t === 12 ? r.i64() : r.u64();
      const hl = r.u64(), lh = r.u64(), ll = r.u64();
      return { t: t === 12 ? 'i256' : 'u256', v: (hh << 192n) | (hl << 128n) | (lh << 64n) | ll };
    }
    case 13: return { t: 'bytes', v: r.var() };
    case 14: return { t: 'string', v: r.str() };
    case 15: return { t: 'symbol', v: r.str(32) };
    case 16: return { t: 'vec', v: r.opt(() => r.arr(next)) ?? [] };
    case 17: return { t: 'map', v: r.opt(() => r.arr(() => ({ k: next(), v: next() }))) ?? [] };
    case 18: return readScAddress(r);
    case 19: {
      const executable = readExecutable(r);
      const storage = r.opt(() => r.arr(() => ({ k: next(), v: next() })));
      return { t: 'instance', executable, storage };
    }
    case 20: return { t: 'instanceKey' };
    case 21: return { t: 'nonceKey', v: r.i64() };
    case 22: return { t: 'executableTag', v: r.str() };
    default: throw new XdrError(`unsupported ScVal type ${t}`);
  }
}

function readExecutable(r) {
  const t = r.u32();
  if (t === 0) return { kind: 'wasm', hash: r.fixed(32) };
  if (t === 1) return { kind: 'stellar-asset' };
  if (t === 2) return { kind: 'external', owner: readScAddress(r), tag: r.str() };
  throw new XdrError(`executable type ${t}`);
}

function readAsset(r) {
  const t = r.u32();
  if (t === 0) return { native: true, code: 'XLM' };
  if (t === 1 || t === 2) {
    const code = td.decode(r.fixed(t === 1 ? 4 : 12)).replace(/\0+$/, '');
    return { native: false, code, issuer: strkeyFor('account', readPublicKey(r)) };
  }
  throw new XdrError(`asset type ${t}`);
}

function readInvokeArgs(r) {
  const contract = readScAddress(r);
  const fn = r.str(32);
  const args = r.arr(() => readScVal(r));
  return { kind: 'call', contract, fn, args };
}

function readPreimage(r) {
  const t = r.u32();
  if (t === 0) return { from: 'address', address: readScAddress(r), salt: r.fixed(32) };
  if (t === 1) return { from: 'asset', asset: readAsset(r) };
  throw new XdrError(`contract id preimage ${t}`);
}

function readCreate(r, v2) {
  const preimage = readPreimage(r);
  const executable = readExecutable(r);
  const args = v2 ? r.arr(() => readScVal(r)) : [];
  return { kind: 'create', preimage, executable, args };
}

function readAuthorizedInvocation(r, depth = 0) {
  if (depth > MAX_DEPTH) throw new XdrError('invocation nested too deeply');
  const [node, raw] = r.span(() => {
    const t = r.u32();
    let fn;
    if (t === 0) fn = readInvokeArgs(r);
    else if (t === 1) fn = readCreate(r, false);
    else if (t === 2) fn = readCreate(r, true);
    else throw new XdrError(`authorized function type ${t}`);
    const subs = r.arr(() => readAuthorizedInvocation(r, depth + 1));
    return { fn, subs };
  });
  node.raw = raw;
  return node;
}

function readAddressCredentials(r) {
  const [addr, addrRaw] = r.span(() => readScAddress(r));
  return { address: addr, addressRaw: addrRaw, nonce: r.i64(), expiration: r.u32(), signature: readScVal(r) };
}

function readDelegate(r, depth = 0) {
  if (depth > MAX_DEPTH) throw new XdrError('delegates nested too deeply');
  return { address: readScAddress(r), signature: readScVal(r), nested: r.arr(() => readDelegate(r, depth + 1)) };
}

function readCredentials(r) {
  const t = r.u32();
  if (t === 0) return { type: 'source-account' };
  if (t === 1) return { type: 'address', ...readAddressCredentials(r) };
  if (t === 2) return { type: 'address-v2', ...readAddressCredentials(r) };
  if (t === 3) {
    const c = readAddressCredentials(r);
    return { type: 'address-with-delegates', ...c, delegates: r.arr(() => readDelegate(r)) };
  }
  throw new XdrError(`credentials type ${t}`);
}

function readAuthEntry(r) {
  const [entry, raw] = r.span(() => ({ credentials: readCredentials(r), root: readAuthorizedInvocation(r) }));
  entry.raw = raw;
  return entry;
}

function readHostFunction(r) {
  const t = r.u32();
  if (t === 0) return readInvokeArgs(r);
  if (t === 1) return readCreate(r, false);
  if (t === 2) return { kind: 'upload', wasm: r.var() };
  if (t === 3) return readCreate(r, true);
  throw new XdrError(`host function type ${t}`);
}

function readMuxed(r) {
  const t = r.u32();
  if (t === 0) return address('account', r.fixed(32));
  if (t === 0x100) { const id = r.u64(); return address('muxed', r.fixed(32), id); }
  throw new XdrError(`muxed account type ${t}`);
}

function readLedgerKey(r) {
  const [key, raw] = r.span(() => {
    const t = r.u32();
    switch (t) {
      case 0: return { type: 'account', account: strkeyFor('account', readPublicKey(r)) };
      case 1: {
        const account = strkeyFor('account', readPublicKey(r));
        const at = r.u32();
        let asset;
        if (at === 3) asset = { pool: hex(r.fixed(32)) };
        else if (at === 0) asset = { native: true, code: 'XLM' };
        else {
          const code = td.decode(r.fixed(at === 1 ? 4 : 12)).replace(/\0+$/, '');
          asset = { code, issuer: strkeyFor('account', readPublicKey(r)) };
        }
        return { type: 'trustline', account, asset };
      }
      case 2: return { type: 'offer', seller: strkeyFor('account', readPublicKey(r)), id: r.i64() };
      case 3: return { type: 'data', account: strkeyFor('account', readPublicKey(r)), name: r.str(64) };
      case 4: { r.u32(); return { type: 'claimable-balance', id: hex(r.fixed(32)) }; }
      case 5: return { type: 'liquidity-pool', id: hex(r.fixed(32)) };
      case 6: return { type: 'contract-data', contract: readScAddress(r), key: readScVal(r), durability: r.u32() ? 'persistent' : 'temporary' };
      case 7: return { type: 'contract-code', hash: hex(r.fixed(32)) };
      case 8: return { type: 'config-setting', id: r.u32() };
      case 9: return { type: 'ttl', hash: hex(r.fixed(32)) };
      default: throw new XdrError(`ledger key type ${t}`);
    }
  });
  key.xdr = toB64(raw);
  return key;
}

function readSorobanData(r) {
  const ext = r.u32();
  const archived = ext === 1 ? r.arr(() => r.u32()) : [];
  if (ext > 1) throw new XdrError(`soroban data ext ${ext}`);
  const readOnly = r.arr(() => readLedgerKey(r));
  const readWrite = r.arr(() => readLedgerKey(r));
  return { readOnly, readWrite, archived, instructions: r.u32(), diskReadBytes: r.u32(), writeBytes: r.u32(), resourceFee: r.i64() };
}

function skipPreconditions(r) {
  const t = r.u32();
  if (t === 0) return null;
  if (t === 1) return { minTime: r.u64(), maxTime: r.u64() };
  if (t !== 2) throw new XdrError(`preconditions ${t}`);
  const tb = r.opt(() => ({ minTime: r.u64(), maxTime: r.u64() }));
  const lb = r.opt(() => ({ minLedger: r.u32(), maxLedger: r.u32() }));
  r.opt(() => r.i64());
  r.u64();
  r.u32();
  r.arr(() => {
    const k = r.u32();
    r.fixed(32);
    if (k === 3) r.var(64);
  }, 2);
  return { ...tb, ...lb };
}

function readMemo(r) {
  const t = r.u32();
  if (t === 0) return null;
  if (t === 1) return { text: r.str(28) };
  if (t === 2) return { id: r.u64() };
  if (t === 3 || t === 4) return { hash: hex(r.fixed(32)) };
  throw new XdrError(`memo ${t}`);
}

function readOperation(r) {
  const source = r.opt(() => readMuxed(r));
  const t = r.u32();
  if (t === 24) return { type: 'invoke', source, hostFunction: readHostFunction(r), auth: r.arr(() => readAuthEntry(r)) };
  if (t === 25) { r.u32(); return { type: 'extend-ttl', source, extendTo: r.u32() }; }
  if (t === 26) { r.u32(); return { type: 'restore', source }; }
  // A classic operation: Soroban transactions carry exactly one operation,
  // so the review stops here and says so instead of guessing.
  throw new XdrError(`operation type ${t} is not a Soroban operation`);
}

function readTransaction(r) {
  const [tx, raw] = r.span(() => {
    const source = readMuxed(r);
    const fee = r.u32();
    const seq = r.i64();
    const cond = skipPreconditions(r);
    const memo = readMemo(r);
    const operations = r.arr(() => readOperation(r), 100);
    const ext = r.u32();
    const soroban = ext === 1 ? readSorobanData(r) : null;
    if (ext > 1) throw new XdrError(`transaction ext ${ext}`);
    return { source, fee, seq, cond, memo, operations, soroban };
  });
  tx.raw = raw;
  return tx;
}

/** Decode a base64 or byte TransactionEnvelope. Fee bumps unwrap to the inner transaction. */
export function decodeEnvelope(xdr) {
  const r = new Reader(bytesOf(xdr));
  const t = r.u32();
  let tx, feeBump = null;
  if (t === 2) {
    tx = readTransaction(r);
  } else if (t === 5) {
    const feeSource = readMuxed(r);
    const fee = r.i64();
    if (r.u32() !== 2) throw new XdrError('fee bump inner envelope type');
    tx = readTransaction(r);
    r.arr(() => { r.fixed(4); r.var(64); }, 20);
    r.u32();
    feeBump = { feeSource, fee };
  } else {
    throw new XdrError(t === 0 ? 'v0 envelopes cannot carry Soroban operations' : `envelope type ${t}`);
  }
  const signatures = r.arr(() => ({ hint: hex(r.fixed(4)), signature: r.var(64) }), 20);
  r.done();
  return { tx, feeBump, signatures };
}

/** Decode one base64 or byte SorobanAuthorizationEntry. */
export function decodeAuthEntry(xdr) {
  const r = new Reader(bytesOf(xdr));
  const e = readAuthEntry(r);
  r.done();
  return e;
}

/** Decode one base64 or byte LedgerKey. */
export function decodeLedgerKey(xdr) {
  const r = new Reader(bytesOf(xdr));
  const k = readLedgerKey(r);
  r.done();
  return k;
}

/** Decode one base64 or byte ScVal. */
export function decodeScVal(xdr) {
  const r = new Reader(bytesOf(xdr));
  const v = readScVal(r);
  r.done();
  return v;
}

/** Decode one base64 or byte ContractEvent. */
export function decodeEvent(xdr) {
  const r = new Reader(bytesOf(xdr));
  r.u32();
  const contract = r.opt(() => address('contract', r.fixed(32)));
  const type = r.u32();
  if (r.u32() !== 0) throw new XdrError('event body version');
  const topics = r.arr(() => readScVal(r));
  const data = readScVal(r);
  r.done();
  return { contract, type, topics, data };
}

/**
 * Decode the parts of a base64 or byte LedgerEntry a preview reads: account
 * and trustline balances, contract data, contract code hashes. Other entry
 * types return only their type.
 */
export function decodeLedgerEntry(xdr) {
  const r = new Reader(bytesOf(xdr));
  const lastModified = r.u32();
  const type = r.u32();
  switch (type) {
    case 0: return { type: 'account', lastModified, account: strkeyFor('account', readPublicKey(r)), balance: r.i64() };
    case 1: {
      const account = strkeyFor('account', readPublicKey(r));
      const at = r.u32();
      let asset;
      if (at === 3) asset = { pool: hex(r.fixed(32)) };
      else if (at === 0) asset = { native: true, code: 'XLM' };
      else asset = { code: td.decode(r.fixed(at === 1 ? 4 : 12)).replace(/\0+$/, ''), issuer: strkeyFor('account', readPublicKey(r)) };
      return { type: 'trustline', lastModified, account, asset, balance: r.i64() };
    }
    case 6: {
      if (r.u32() !== 0) throw new XdrError('contract data ext');
      const contract = readScAddress(r);
      const key = readScVal(r);
      const durability = r.u32() ? 'persistent' : 'temporary';
      return { type: 'contract-data', lastModified, contract, key, durability, val: readScVal(r) };
    }
    case 7: {
      // ContractCodeEntryExt v1: ExtensionPoint, then ContractCodeCostInputs
      // (its own ExtensionPoint and ten counters).
      const ext = r.u32();
      if (ext === 1) r.fixed(12 * 4);
      else if (ext !== 0) throw new XdrError(`contract code ext ${ext}`);
      const hash = hex(r.fixed(32));
      return { type: 'contract-code', lastModified, hash, code: r.var() };
    }
    default: return { type: ['account', 'trustline', 'offer', 'data', 'claimable-balance', 'liquidity-pool', 'contract-data', 'contract-code', 'config-setting', 'ttl'][type] ?? `type-${type}`, lastModified };
  }
}

// ---------------------------------------------------------------------------
// StrKey, hashing, preimages.
// ---------------------------------------------------------------------------

const B32 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';

function crc16(bytes) {
  let crc = 0;
  for (const b of bytes) {
    crc ^= b << 8;
    for (let i = 0; i < 8; i++) crc = crc & 0x8000 ? ((crc << 1) ^ 0x1021) & 0xffff : (crc << 1) & 0xffff;
  }
  return crc;
}

function strkey(version, payload) {
  const data = new Uint8Array(payload.length + 3);
  data[0] = version;
  data.set(payload, 1);
  const c = crc16(data.subarray(0, payload.length + 1));
  data[payload.length + 1] = c & 0xff;
  data[payload.length + 2] = c >> 8;
  let bits = 0, value = 0, out = '';
  for (const b of data) {
    value = (value << 8) | b;
    bits += 8;
    while (bits >= 5) { out += B32[(value >>> (bits - 5)) & 31]; bits -= 5; }
  }
  if (bits > 0) out += B32[(value << (5 - bits)) & 31];
  return out;
}

/**
 * The 32-byte payload of a G… or C… StrKey. Throws on a bad alphabet,
 * version byte or checksum.
 */
export function fromStrkey(s) {
  const versions = { G: 6 << 3, C: 2 << 3 };
  let bits = 0, n = 0;
  const out = [];
  for (const c of String(s)) {
    const x = B32.indexOf(c);
    if (x < 0) throw new XdrError(`not a StrKey: ${s}`);
    bits = (bits << 5) | x;
    n += 5;
    if (n >= 8) { n -= 8; out.push((bits >> n) & 255); }
  }
  const data = Uint8Array.from(out);
  if (data.length !== 35 || data[0] !== versions[s[0]]) throw new XdrError(`not an account or contract StrKey: ${s}`);
  const c = crc16(data.subarray(0, 33));
  if (data[33] !== (c & 0xff) || data[34] !== c >> 8) throw new XdrError(`bad StrKey checksum: ${s}`);
  return data.subarray(1, 33);
}

export function strkeyFor(kind, key, id) {
  switch (kind) {
    case 'account': return strkey(6 << 3, key);
    case 'contract': return strkey(2 << 3, key);
    case 'muxed': {
      const p = new Uint8Array(40);
      p.set(key);
      new DataView(p.buffer).setBigUint64(32, id);
      return strkey(12 << 3, p);
    }
    case 'claimable': { const p = new Uint8Array(33); p.set(key, 1); return strkey(1 << 3, p); }
    case 'pool': return strkey(11 << 3, key);
    default: return hex(key);
  }
}

export const sha256 = async (bytes) => new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
const u32be = (n) => { const b = new Uint8Array(4); new DataView(b.buffer).setUint32(0, n); return b; };
const i64be = (n) => { const b = new Uint8Array(8); new DataView(b.buffer).setBigInt64(0, BigInt(n)); return b; };
const concat = (...parts) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) { out.set(p, o); o += p.length; }
  return out;
};

export const NETWORKS = {
  testnet: 'Test SDF Network ; September 2015',
  mainnet: 'Public Global Stellar Network ; September 2015',
  futurenet: 'Test SDF Future Network ; October 2022',
  local: 'Standalone Network ; February 2017',
};

export const networkId = (passphrase) => sha256(new TextEncoder().encode(passphrase));

/** The native asset's Stellar Asset Contract id on a network. */
export async function nativeAssetContract(passphrase) {
  // HashIdPreimage::ContractId { networkId, ContractIdPreimage::FromAsset(Asset::Native) }
  const pre = concat(u32be(8), await networkId(passphrase), u32be(1), u32be(0));
  return strkeyFor('contract', await sha256(pre));
}

/**
 * The 32-byte payload an address credential signs, for a given expiration
 * ledger: sha256(HashIdPreimage::SorobanAuthorization[WithAddress]).
 */
export async function authPayloadHash(entry, passphrase, expirationLedger) {
  const c = entry.credentials;
  if (!c.address) return null;
  const nid = await networkId(passphrase);
  const exp = u32be(expirationLedger ?? c.expiration);
  const pre = c.type === 'address'
    ? concat(u32be(9), nid, i64be(c.nonce), exp, entry.root.raw)
    : concat(u32be(10), nid, i64be(c.nonce), exp, c.addressRaw, entry.root.raw);
  return sha256(pre);
}

/** The transaction hash the source account signs. */
export async function transactionHash(decoded, passphrase) {
  return sha256(concat(await networkId(passphrase), u32be(2), decoded.tx.raw));
}

// ---------------------------------------------------------------------------
// Readable labels.
// ---------------------------------------------------------------------------

// SEP-41 token interface (and the Stellar Asset Contract's admin calls):
// argument names, used only for contracts known to be tokens.
export const TOKEN_FNS = {
  transfer: ['from', 'to', 'amount'],
  transfer_from: ['spender', 'from', 'to', 'amount'],
  approve: ['from', 'spender', 'amount', 'expiration_ledger'],
  burn: ['from', 'amount'],
  burn_from: ['spender', 'from', 'amount'],
  mint: ['to', 'amount'],
  clawback: ['from', 'amount'],
  set_admin: ['new_admin'],
  set_authorized: ['id', 'authorize'],
};

// Perch account entry points that carry a policy document.
export const PERCH_FNS = {
  apply_doc: ['doc_json', 'approval_valid_until', 'expected_revision'],
  apply_scope: ['grant_id', 'doc_json'],
  remove_scope: ['grant_id'],
  execute: ['target', 'target_fn', 'target_args'],
};

/**
 * The signer's outgoing payment in one call: a SEP-41 `transfer` from the
 * signer on a contract `tokenOf` knows as a token, else null.
 */
export function paymentOf(fn, signer, tokenOf) {
  if (fn.kind !== 'call' || fn.fn !== 'transfer' || fn.args.length !== 3 || !signer) return null;
  const token = tokenOf(fn.contract.str);
  const [from, to, amount] = fn.args;
  if (!token || from?.str !== signer || !/128|64/.test(amount?.t ?? '')) return null;
  return { token, amount: amount.v, to };
}

/** How many sub-call payments one summary line names before it counts the rest. */
export const SUMMARY_PAYMENTS = 3;

/**
 * What one summary line says about an authorized call's sub-calls: the
 * signer's payments, depth first, up to `max`, and how many other sub-calls
 * (other payments included) it leaves to Details.
 */
export function subCallSummary(root, signer, tokenOf, max = SUMMARY_PAYMENTS) {
  const payments = [];
  let more = 0;
  const walk = (inv) => {
    const p = paymentOf(inv.fn, signer, tokenOf);
    if (p && payments.length < max) payments.push(p);
    else more++;
    inv.subs.forEach(walk);
  };
  root.subs.forEach(walk);
  return { payments, more };
}

export function formatAmount(v, decimals = 7) {
  const neg = v < 0n;
  let s = (neg ? -v : v).toString().padStart(decimals + 1, '0');
  const whole = s.slice(0, s.length - decimals).replace(/\B(?=(\d{3})+(?!\d))/g, ',');
  const frac = decimals ? s.slice(s.length - decimals).replace(/0+$/, '') : '';
  return (neg ? '−' : '') + whole + (frac ? '.' + frac : '');
}

export const short = (s) => (s && s.length > 14 ? `${s.slice(0, 4)}…${s.slice(-4)}` : s);

export function tryJson(bytes) {
  if (!bytes || bytes.length < 2 || bytes[0] !== 0x7b) return null;
  try { return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes)); } catch { return null; }
}

// ---------------------------------------------------------------------------
// Authorization tree comparison (supplied vs independently recorded).
// ---------------------------------------------------------------------------

const sameBytes = (a, b) => a.length === b.length && a.every((x, i) => x === b[i]);
const credAddress = (e) => e.credentials.address?.str ?? 'source-account';

/**
 * Pair supplied entries with recorded ones. An entry matches when the signer
 * and the whole invocation tree are byte-identical; nonces, expirations and
 * signatures are ignored because the recording invents its own.
 */
export function compareAuth(supplied, recorded) {
  const left = recorded.map((e, i) => ({ e, i, used: false }));
  const result = supplied.map((e, index) => {
    const exact = left.find((x) => !x.used && credAddress(x.e) === credAddress(e) && sameBytes(x.e.root.raw, e.root.raw));
    if (exact) { exact.used = true; return { index, status: 'match', recorded: exact.i }; }
    return { index, status: 'unmatched' };
  });
  for (const r of result) {
    if (r.status !== 'unmatched') continue;
    const e = supplied[r.index];
    const same = left.find((x) => !x.used && credAddress(x.e) === credAddress(e));
    if (same) { same.used = true; r.status = 'differs'; r.recorded = same.i; } else r.status = 'extra';
  }
  const missing = left.filter((x) => !x.used).map((x) => x.i);
  return { entries: result, missing };
}
