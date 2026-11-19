// <soroban-sign-dialog>: review a Soroban request before a wallet signs it.
//
// The authorization tree is decoded from XDR alone, so it renders instantly
// with no network and no simulator, on any protocol. "Preview effects" is
// opt-in: it calls a preview provider (by default the engine module named by
// the `engine` attribute, imported on first use) that re-records the tree
// independently and reports balance changes, events, the account's verdict
// and any spending budget.
//
// The component never holds keys. Approval emits the exact entries to sign,
// with the hash of each reviewed invocation, and the wallet signs them.
//
// Importing this module has no side effects and touches no browser global
// beyond reading `HTMLElement`: call `define()` (or `request()`, which
// defines on first use) to register the element.

import {
  XdrError, decodeEnvelope, decodeAuthEntry, decodeEvent, compareAuth, transactionHash, nativeAssetContract,
  formatAmount, NETWORKS, TOKEN_FNS, PERCH_FNS, toB64, hex, sha256, short, tryJson, paymentOf, subCallSummary,
} from './xdr.js';
import { mergeStrings } from './strings.js';
import { STYLE } from './style.js';

export const TAG = 'soroban-sign-dialog';

const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

// One stylesheet for every instance, built on first use. Constructable
// stylesheets need no `style-src 'unsafe-inline'`; browsers without them get
// a <style> element instead.
let sheet = null;
function adoptStyle(root) {
  if (typeof CSSStyleSheet === 'function' && 'adoptedStyleSheets' in root) {
    try {
      if (!sheet) { sheet = new CSSStyleSheet(); sheet.replaceSync(STYLE); }
      root.adoptedStyleSheets = [sheet];
      return;
    } catch { /* fall through */ }
  }
  const style = document.createElement('style');
  style.textContent = STYLE;
  root.append(style);
}

const Base = globalThis.HTMLElement ?? class {};

export class SorobanSignDialog extends Base {
  static observedAttributes = ['transaction', 'auth-entries', 'network', 'signer', 'dapp', 'open', 'preview', 'engine', 'rpc-url', 'strict', 'details'];

  #root;
  #labels = new Map();
  #tokens = new Map();
  #strings = mergeStrings();
  #provider = null;
  #decoded = null;
  #entries = [];
  #error = null;
  #preview = null;
  #previewState = 'idle';
  #comparison = null;
  #pending = null;
  #confirming = false;
  #nativeSac = null;
  #renderQueued = false;
  #abort = null;
  #approval = null;
  #detailsOpen = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adoptStyle(this.#root);
    this.dialog = document.createElement('dialog');
    this.dialog.setAttribute('part', 'dialog');
    this.dialog.setAttribute('aria-labelledby', 'ssd-title');
    this.dialog.setAttribute('aria-describedby', 'ssd-what ssd-status');
    this.#root.append(this.dialog);
    this.dialog.addEventListener('cancel', (e) => { e.preventDefault(); this.#finish('dismissed'); });
    this.dialog.addEventListener('click', (e) => this.#onClick(e));
    this.dialog.addEventListener('keydown', (e) => this.#trapFocus(e));
  }

  connectedCallback() {
    this.#queue();
    if (this.hasAttribute('open')) queueMicrotask(() => this.show());
  }

  attributeChangedCallback(name, oldValue, value) {
    if (oldValue === value) return;
    if (name === 'transaction' || name === 'auth-entries' || name === 'network' || name === 'signer') this.#reset();
    if (name === 'open') {
      if (value !== null && !this.dialog.open) this.show();
      if (value === null && this.dialog.open) this.#finish('closed');
    }
    this.#queue();
  }

  // --- properties -----------------------------------------------------------

  /** Base64 TransactionEnvelope under review. */
  get transaction() { return this.getAttribute('transaction'); }
  set transaction(v) { v ? this.setAttribute('transaction', v) : this.removeAttribute('transaction'); }

  /** Base64 SorobanAuthorizationEntry list, for a bare auth-entry request. */
  get authEntries() { const a = this.getAttribute('auth-entries'); return a ? a.split(/[\s,]+/).filter(Boolean) : []; }
  set authEntries(list) { list?.length ? this.setAttribute('auth-entries', list.join(' ')) : this.removeAttribute('auth-entries'); }

  /** `testnet`, `mainnet`, `futurenet`, `local` or a passphrase; reads back the passphrase. */
  get network() { return this.networkPassphrase; }
  set network(v) { this.setAttribute('network', v); }
  get networkPassphrase() {
    const n = this.getAttribute('network') ?? 'testnet';
    return NETWORKS[n] ?? n;
  }
  set networkPassphrase(v) { this.setAttribute('network', v); }

  /** Address the wallet signs for; its entries are marked and `forSigner`. */
  get signer() { return this.getAttribute('signer'); }
  set signer(v) { v ? this.setAttribute('signer', v) : this.removeAttribute('signer'); }

  /** The requesting dapp's name or origin, shown in the header. */
  get dapp() { return this.getAttribute('dapp'); }
  set dapp(v) { v ? this.setAttribute('dapp', v) : this.removeAttribute('dapp'); }

  get engine() { return this.getAttribute('engine'); }
  set engine(v) { v ? this.setAttribute('engine', v) : this.removeAttribute('engine'); }

  get rpcUrl() { return this.getAttribute('rpc-url'); }
  set rpcUrl(v) { v ? this.setAttribute('rpc-url', v) : this.removeAttribute('rpc-url'); }

  get strict() { return this.hasAttribute('strict'); }
  set strict(v) { this.toggleAttribute('strict', !!v); }

  /**
   * Names for addresses: { "C…": "Café" } or { "C…": { name, token: { code, decimals } } }.
   * Labels come from the embedder (wallet address book, dapp manifest); the
   * component adds only the native asset contract, which it derives itself.
   */
  get labels() {
    return Object.fromEntries([...this.#labels].map(([k, name]) => [k, this.#tokens.has(k) ? { name, token: this.#tokens.get(k) } : name]));
  }
  set labels(map) {
    this.#labels = new Map();
    this.#tokens = new Map();
    for (const [k, v] of Object.entries(map ?? {})) {
      if (typeof v === 'string') this.#labels.set(k, v);
      else if (v) { if (v.name) this.#labels.set(k, v.name); if (v.token) this.#tokens.set(k, v.token); }
    }
    this.#queue();
  }

  /** User-facing strings; overrides are merged over the English defaults. */
  get strings() { return this.#strings; }
  set strings(overrides) { this.#strings = mergeStrings(overrides ?? {}); this.#queue(); }

  /**
   * async (request) => PreviewResult. The default imports the module named by
   * the `engine` attribute on first use and calls its `preview(request)`.
   */
  get previewProvider() { return this.#provider; }
  set previewProvider(fn) { this.#provider = fn; this.#queue(); queueMicrotask(() => this.#autoPreview()); }

  get previewResult() { return this.#preview; }

  /** Flags raised by the latest preview. */
  get flags() { return this.#flags(); }

  get open() { return !!this.dialog?.open; }

  // --- methods --------------------------------------------------------------

  show() {
    if (!this.isConnected) document.body.append(this);
    this.#render();
    if (this.dialog.open) return;
    if (this.hasAttribute('inline')) this.dialog.show();
    else this.dialog.showModal();
    this.#root.querySelector('[data-act="reject"]')?.focus();
    this.dispatchEvent(new CustomEvent('open', { bubbles: true, composed: true }));
    this.#autoPreview();
  }

  /** `preview` is `auto` (the default), `manual` or `off`. */
  get #previewMode() { return this.getAttribute('preview') ?? 'auto'; }

  /** The preview settled without simulating: an error, a fallback or any status other than ok. */
  get #unsimulated() { return !['idle', 'loading', 'done'].includes(this.#previewState); }

  get #canPreview() { return !!(this.#provider || this.hasAttribute('engine')) && this.#previewMode !== 'off'; }

  #autoPreview() {
    if (this.dialog?.open && this.#previewMode === 'auto' && this.#canPreview && this.#previewState === 'idle') this.runPreview();
  }

  close() { this.#finish('closed'); }

  /**
   * Open the dialog for one request and resolve when the user decides:
   * { approved: true, ...approveDetail } or { approved: false, reason }.
   */
  request(opts = {}) {
    if (opts.labels) this.labels = opts.labels;
    if (opts.strings) this.strings = opts.strings;
    if (opts.previewProvider) this.previewProvider = opts.previewProvider;
    // Per-request fields: what this request leaves out is cleared, so nothing
    // carries over from the previous one.
    const perRequest = {
      network: opts.network ?? opts.networkPassphrase,
      signer: opts.signer,
      dapp: opts.dapp,
      transaction: opts.authEntries?.length ? null : opts.transaction,
      'auth-entries': opts.authEntries?.join(' '),
    };
    for (const [attr, v] of Object.entries(perRequest)) v ? this.setAttribute(attr, v) : this.removeAttribute(attr);
    // Dialog-level configuration persists until a request sets it again.
    for (const [k, attr] of [['engine', 'engine'], ['rpcUrl', 'rpc-url'], ['theme', 'theme'], ['preview', 'preview']]) {
      if (opts[k]) this.setAttribute(attr, opts[k]);
    }
    if (opts.strict != null) this.strict = opts.strict;
    this.#reset();
    this.#pending?.resolve({ approved: false, reason: 'superseded' });
    return new Promise((resolve) => {
      this.#pending = { resolve };
      this.show();
    });
  }

  async runPreview() {
    if (this.#previewState === 'loading' || !this.#decoded) return;
    this.#abort?.abort();
    const abort = (this.#abort = new AbortController());
    this.#previewState = 'loading';
    this.#render();
    try {
      const provider = this.#provider ?? (await this.#engineProvider());
      const result = await provider({
        transaction: this.transaction,
        authEntries: this.#entries.map((e) => toB64(e.raw)),
        networkPassphrase: this.networkPassphrase,
        signer: this.signer,
        rpcUrl: this.getAttribute('rpc-url'),
        decoded: this.#decoded,
        signal: abort.signal,
      });
      if (abort.signal.aborted) return;
      this.#setPreview(result);
    } catch (err) {
      if (err?.name === 'AbortError' || abort.signal.aborted) return;
      this.#setPreview({ status: 'error', code: 'error', params: { error: err?.message ?? String(err) }, notice: `Preview failed: ${err?.message ?? err}` });
    }
  }

  // --- internals ------------------------------------------------------------

  async #engineProvider() {
    const url = this.getAttribute('engine');
    if (!url) throw new Error('no preview engine configured (set the engine attribute or previewProvider)');
    const mod = await import(/* @vite-ignore */ new URL(url, document.baseURI).href);
    return (req) => mod.preview(req);
  }

  #setPreview(result) {
    this.#preview = result;
    this.#previewState = result?.status === 'ok' ? 'done' : (result?.status ?? 'error');
    this.#comparison = null;
    if (result?.recordedAuth) {
      try {
        const recorded = result.recordedAuth.map(decodeAuthEntry);
        this.#comparison = compareAuth(this.#entries, recorded);
        this.#comparison.recorded = recorded;
      } catch (err) {
        result.notice = `${result.notice ?? ''} Recorded tree could not be decoded: ${err.message}`.trim();
      }
    }
    this.#confirming = false;
    this.#render();
    this.dispatchEvent(new CustomEvent('preview', { bubbles: true, composed: true, detail: { result, flags: this.#flags() } }));
  }

  #reset() {
    this.#decoded = null;
    this.#entries = [];
    this.#error = null;
    this.#preview = null;
    this.#comparison = null;
    this.#previewState = 'idle';
    this.#confirming = false;
    this.#detailsOpen = null;
    this.#abort?.abort();
    try {
      if (this.transaction) {
        const env = decodeEnvelope(this.transaction);
        const op = env.tx.operations[0];
        if (env.tx.operations.length !== 1) throw new XdrError('a Soroban transaction carries exactly one operation');
        this.#decoded = { kind: 'transaction', envelope: env, op };
        this.#entries = op.type === 'invoke' ? op.auth : [];
      } else if (this.authEntries.length) {
        this.#entries = this.authEntries.map(decodeAuthEntry);
        this.#decoded = { kind: 'auth-entries' };
      }
    } catch (err) {
      this.#error = err;
    }
    this.#approval = this.#error || !this.#decoded ? null : this.#approvalDetail();
    this.#approval?.catch(() => {});
    if (this.dialog?.open) queueMicrotask(() => this.#autoPreview());
    nativeAssetContract(this.networkPassphrase).then((id) => {
      if (id !== this.#nativeSac) { this.#nativeSac = id; this.#queue(); }
    }, () => {});
  }

  #queue() {
    if (this.#renderQueued) return;
    this.#renderQueued = true;
    queueMicrotask(() => { this.#renderQueued = false; this.#render(); });
  }

  #flags() {
    const flags = [];
    const p = this.#preview;
    if (this.#unsimulated) flags.push({ kind: 'not-simulated', code: p?.code ?? null });
    if (this.#comparison) {
      for (const e of this.#comparison.entries) if (e.status !== 'match') flags.push({ kind: e.status, entry: e.index });
      for (const i of this.#comparison.missing) flags.push({ kind: 'missing', recorded: i });
    }
    for (const v of p?.verdicts ?? []) if (v.allowed === false) flags.push({ kind: 'refused', address: v.address, code: v.code });
    if (p?.outcome && !p.outcome.ok && !(p.verdicts ?? []).some((v) => v.allowed === false)) flags.push({ kind: 'fails', error: p.outcome.error });
    return flags;
  }

  #t(key, params = {}) {
    const v = this.#strings[key];
    return typeof v === 'function' ? v(params) : v;
  }

  /** A notice or reason by code, else the provider's own text. */
  #coded(table, code, params, fallback) {
    const v = code != null ? this.#strings[table]?.[code] : undefined;
    if (v == null) return esc(fallback ?? '');
    const escaped = Object.fromEntries(Object.entries(params ?? {}).map(([k, x]) => [k, Array.isArray(x) ? x.map(esc) : x == null ? x : esc(x)]));
    return typeof v === 'function' ? v(escaped) : v;
  }

  #label(addr) {
    if (!addr) return '';
    if (addr === this.#nativeSac) return this.#labels.get(addr) ?? this.#t('nativeAsset');
    return this.#labels.get(addr) ?? null;
  }

  #token(addr) {
    if (addr === this.#nativeSac) return this.#tokens.get(addr) ?? { code: 'XLM', decimals: 7 };
    return this.#tokens.get(addr) ?? null;
  }

  /** A provider's token info, or the element's labels for a contract it did not know. */
  #assetToken(t) {
    return (t?.contract && this.#token(t.contract)) || t || { code: '', decimals: 0 };
  }

  #addr(a) {
    const s = typeof a === 'string' ? a : a?.str ?? '';
    const label = this.#label(s);
    const you = (s && s === this.signer ? ` (${esc(this.#t('you'))})` : '') + (a?.muxId != null ? ` #${esc(a.muxId)}` : '');
    return label
      ? `<span class="addr" title="${esc(s)}">${esc(label)}${you} <span class="mono muted">${esc(short(s))}</span></span>`
      : `<span class="addr mono" title="${esc(s)}">${esc(short(s))}${you}</span>`;
  }

  #amount(v, token) {
    return `${esc(formatAmount(v, token.decimals))} ${esc(token.code)}`;
  }

  #val(v, hint, token) {
    switch (v.t) {
      case 'address': return this.#addr(v);
      case 'i128': case 'u128': case 'i256': case 'u256': case 'i64': case 'u64':
        if (token && /amount|limit/.test(hint ?? '')) return `<b>${this.#amount(v.v, token)}</b> <span class="muted small">(${esc(v.v.toString())})</span>`;
        return `<span class="mono">${esc(v.v.toString())}</span>`;
      case 'u32': case 'i32': return `<span class="mono">${v.v}</span>`;
      case 'timepoint': return esc(new Date(Number(v.v) * 1000).toISOString());
      case 'duration': return this.#t('seconds', { value: esc(v.v.toString()) });
      case 'bool': return this.#t(v.v ? 'yes' : 'no');
      case 'void': return `<span class="muted">${this.#t('none')}</span>`;
      case 'symbol': return `<code>${esc(v.v)}</code>`;
      case 'string': return `“${esc(v.v)}”`;
      case 'bytes': {
        const doc = tryJson(v.v);
        if (doc) return this.#doc(doc);
        const h = hex(v.v);
        return `<span class="mono" title="${esc(h)}">0x${esc(h.length > 32 ? h.slice(0, 32) + '…' : h)}</span> <span class="muted small">(${this.#t('bytes', { length: v.v.length })})</span>`;
      }
      case 'vec': return v.v.length ? `[${v.v.map((x) => this.#val(x)).join(', ')}]` : '[ ]';
      case 'map': return `{ ${v.v.map((e) => `${this.#val(e.k)}: ${this.#val(e.v, e.k.v)}`).join(', ')} }`;
      case 'error': return `<span class="mono">error(${v.type}, ${v.code})</span>`;
      default: return `<span class="mono">${esc(v.t)}</span>`;
    }
  }

  /** A policy document carried in bytes (an account policy or grant install). */
  #doc(doc) {
    const rules = Array.isArray(doc.rules) ? doc.rules : [];
    const signers = Array.isArray(doc.signers) ? doc.signers : [];
    const items = rules.map((rule) => {
      const scope = rule.scope?.type === 'contract' ? this.#addr(String(rule.scope.address)) : esc(rule.scope?.type ?? 'any');
      const functions = Array.isArray(rule.functions) ? rule.functions.map((f) => `<code>${esc(f)}</code>`).join(', ') : this.#t('anyFunction');
      let cap = null;
      if (rule.cap?.limit) {
        const token = this.#token(rule.cap.token ?? rule.scope?.address) ?? { code: 'units', decimals: 0 };
        try { cap = this.#amount(BigInt(rule.cap.limit), token); } catch { cap = esc(rule.cap.limit); }
      }
      const by = Array.isArray(rule.principals?.signers) ? rule.principals.signers.map((s) => `<code>${esc(s)}</code>`).join(' + ') : null;
      return `<li>${this.#t('ruleLine', { name: esc(rule.name ?? 'rule'), functions, scope, by, cap, period: esc(rule.cap?.['period-ledgers'] ?? '?') })}</li>`;
    }).join('');
    const keys = signers.map((s) => `<li><code>${esc(s.id)}</code>: ${s.key ? this.#t('signerKey', { key: `<span class="mono">${esc(short(String(s.key)))}</span>` }) : this.#addr(String(s.address ?? ''))}</li>`).join('');
    return `<div class="doc" part="doc"><div class="small muted">${this.#t('policyDocument', { network: doc.network == null ? null : esc(doc.network) })}</div>
      ${items ? `<div class="small">${this.#t('rules')}</div><ul>${items}</ul>` : ''}${keys ? `<div class="small">${this.#t('signers')}</div><ul>${keys}</ul>` : ''}</div>`;
  }

  #sentence(fn) {
    if (fn.kind === 'create') {
      if (fn.preimage.from === 'asset') return this.#t('deployAsset', { code: esc(fn.preimage.asset.code) });
      return this.#t('deploy', { code: fn.executable.kind === 'wasm' ? esc(short(hex(fn.executable.hash))) : null });
    }
    if (fn.kind === 'upload') return this.#t('upload', { length: fn.wasm.length });
    const c = fn.contract.str;
    const token = this.#token(c);
    const names = token ? TOKEN_FNS[fn.fn] : null;
    if (token && names && fn.args.length === names.length) {
      const arg = (n) => fn.args[names.indexOf(n)];
      const amount = (v) => (v && /128|64/.test(v.t) ? this.#amount(v.v, token) : '?');
      const a = (n) => this.#addr(arg(n));
      switch (fn.fn) {
        case 'transfer': return this.#t('send', { amount: amount(arg('amount')), from: a('from'), to: a('to') });
        case 'approve': return this.#t('approve', { spender: a('spender'), amount: amount(arg('amount')), from: a('from'), until: esc(arg('expiration_ledger').v) });
        case 'transfer_from': return this.#t('transferFrom', { spender: a('spender'), amount: amount(arg('amount')), from: a('from'), to: a('to') });
        case 'burn': return this.#t('burn', { amount: amount(arg('amount')), from: a('from') });
        case 'mint': return this.#t('mint', { amount: amount(arg('amount')), to: a('to') });
        default: break;
      }
    }
    if (c === this.signer && fn.fn === 'apply_scope') return this.#t('installGrant', { grant: esc(fn.args[0]?.v ?? '?') });
    if (c === this.signer && fn.fn === 'apply_doc') return this.#t('replacePolicy');
    return this.#t('call', { fn: esc(fn.fn), contract: this.#addr(fn.contract) });
  }

  #argNames(fn) {
    if (fn.kind !== 'call') return [];
    if (this.#token(fn.contract.str) && TOKEN_FNS[fn.fn]?.length === fn.args.length) return TOKEN_FNS[fn.fn];
    if (fn.contract.str === this.signer && PERCH_FNS[fn.fn]?.length === fn.args.length) return PERCH_FNS[fn.fn];
    return [];
  }

  #node(inv) {
    const fn = inv.fn;
    const names = this.#argNames(fn);
    const token = fn.kind === 'call' ? this.#token(fn.contract.str) : null;
    const args = fn.args.map((a, i) => `<dt>${names[i] ? esc(names[i]) : this.#t('argument', { index: i + 1 })}</dt><dd>${this.#val(a, names[i], token)}</dd>`).join('');
    const where = fn.kind === 'call' ? `<code>${esc(fn.fn)}</code> · ${esc(this.#label(fn.contract.str) ?? short(fn.contract.str))}` : esc(fn.kind);
    const count = fn.args.length ? ` · ${this.#t('argumentCount', { count: fn.args.length })}` : '';
    const details = `<details part="node-details"><summary>${where}${count}</summary>
      ${fn.kind === 'call' ? `<dl class="args"><dt>${this.#t('contract')}</dt><dd class="mono">${esc(fn.contract.str)}</dd>${args}</dl>` : args ? `<dl class="args">${args}</dl>` : ''}</details>`;
    const kids = inv.subs.length ? `<ol role="list">${inv.subs.map((s) => this.#node(s)).join('')}</ol>` : '';
    return `<li class="node" part="node"><div class="sentence">${this.#sentence(fn)}</div>${details}${kids}</li>`;
  }

  #entry(e, i) {
    const c = e.credentials;
    const who = c.type === 'source-account' ? this.#t('sourceAccount') : this.#addr(c.address);
    const cmp = i >= 0 ? this.#comparison?.entries[i] : null;
    const badge = !cmp ? ''
      : cmp.status === 'match' ? `<span class="badge ok">${this.#t('badgeMatch')}</span>`
      : cmp.status === 'extra' ? `<span class="badge bad">${this.#t('badgeExtra')}</span>`
      : `<span class="badge bad">${this.#t('badgeDiffers')}</span>`;
    const note = cmp?.status === 'extra' ? `<div class="notice bad small" part="unused-entry">${this.#t('extraNote')}</div>`
      : cmp?.status === 'differs' ? `<div class="notice bad small">${this.#t('differsNote')}</div>` : '';
    const mine = !!c.address && c.address.str === this.signer;
    const head = i >= 0 ? this.#t('authorizedBy', { index: i + 1, who }) : `<b>${who}</b>`;
    return `<article class="entry" part="entry" data-status="${cmp?.status ?? 'unchecked'}" data-mine="${mine}">
      <div class="entry-head"><span>${head}${c.type === 'address-v2' ? ` · ${this.#t('addressBound')}` : ''}</span>${badge}</div>
      ${note}<ol class="tree" role="list">${this.#node(e.root)}</ol></article>`;
  }

  /** The preview's evidence, for the Details disclosure. */
  #effectsHtml() {
    const p = this.#preview;
    if (this.#previewState !== 'done') return '';
    const parts = [];
    if (p.notice || (p.code && this.#strings.notices[p.code])) parts.push(`<div class="notice" part="notice">${this.#coded('notices', p.code, p.params, p.notice)}</div>`);
    const cmp = this.#comparison;
    if (cmp) {
      const count = (s) => cmp.entries.filter((e) => e.status === s).length;
      const bad = cmp.entries.length - count('match') + cmp.missing.length;
      parts.push(bad
        ? `<div class="notice bad" part="auth-check">${this.#t('authMismatch', { extra: count('extra'), differs: count('differs'), missing: cmp.missing.length })}</div>`
        : `<div class="notice ok" part="auth-check">${this.#t('authMatch')}</div>`);
      if (cmp.missing.length) parts.push(`<div class="small">${this.#t('neededNotSupplied')}</div>${cmp.missing.map((i) => this.#entry(cmp.recorded[i], -1)).join('')}`);
    }
    for (const v of p.verdicts ?? []) {
      const params = { account: this.#addr(v.address), rule: v.rule ? esc(v.rule.name ?? v.rule.id) : null };
      const reason = v.reasonCode || v.reason ? this.#coded('reasons', v.reasonCode, v.params, v.reason) : '';
      const known = typeof v.allowed === 'boolean';
      parts.push(`<div class="card verdict" part="verdict" data-allowed="${known ? v.allowed : 'null'}"><span class="icon" aria-hidden="true">${!known ? '?' : v.allowed ? '✓' : '✕'}</span>
        <div>${this.#t(!known ? 'verdictUnknown' : v.allowed ? 'wouldAccept' : 'wouldRefuse', params)}
        ${reason ? `<div class="small">${reason}</div>` : v.code != null ? `<div class="small muted">${this.#t('contractError', { code: esc(v.code) })}</div>` : ''}</div></div>`);
    }
    for (const b of p.budgets ?? []) parts.push(this.#budget(b));
    if (p.balances?.length) {
      const rows = p.balances.map((b) => {
        const before = BigInt(b.before ?? 0), after = BigInt(b.after ?? 0), d = after - before;
        const tok = this.#assetToken(b.asset);
        return `<tr><td>${this.#addr(b.address)}</td><td>${esc(tok.code)}</td><td class="num">${esc(formatAmount(before, tok.decimals))}</td><td class="num">${esc(formatAmount(after, tok.decimals))}</td><td class="num ${d < 0n ? 'neg' : 'pos'}">${d > 0n ? '+' : ''}${esc(formatAmount(d, tok.decimals))}</td></tr>`;
      }).join('');
      const h = this.#strings.balanceHeaders;
      parts.push(`<h3>${this.#t('balanceChanges')}</h3><div class="scroll"><table part="balances"><thead><tr><th>${h[0]}</th><th>${h[1]}</th><th class="num">${h[2]}</th><th class="num">${h[3]}</th><th class="num">${h[4]}</th></tr></thead><tbody>${rows}</tbody></table></div>`);
    } else if (p.outcome?.ok) {
      parts.push(`<p class="small muted">${this.#t('noBalanceChanges')}</p>`);
    }
    if (p.events?.length) {
      const items = p.events.map((x) => { try { return `<li>${this.#event(decodeEvent(x))}</li>`; } catch (err) { return `<li class="muted">${this.#t('undecodableEvent', { error: esc(err.message) })}</li>`; } }).join('');
      parts.push(`<h3>${this.#t('events')}</h3><ul class="events" part="events">${items}</ul>`);
    }
    if (p.outcome && !p.outcome.ok && !(p.verdicts ?? []).length) {
      const codes = (p.outcome.codes ?? []).map((c) => `${esc(c.code)}${c.contract ? ` (${this.#addr(c.contract)})` : ''}`).join(', ');
      parts.push(`<div class="notice bad">${this.#t('wouldFail', { error: esc(p.outcome.error ?? 'unknown error'), codes: codes || null })}</div>`);
    }
    parts.push(`<p class="small muted">${this.#t('footnote', { ledger: p.ledger ? esc(p.ledger.sequence) : null, protocol: esc(p.ledger?.protocol ?? p.engine?.protocol ?? '?'), source: esc(p.stateSource ?? 'RPC') })}</p>`);
    return `<section class="preview" part="preview"><h3>${this.#t('effects')}</h3>${parts.join('')}</section>`;
  }

  /** One status for the simulated outcome: success, refused, unknown, checking or idle. */
  #outcome() {
    const s = this.#previewState, p = this.#preview;
    if (s === 'loading') return { state: 'checking' };
    if (s === 'idle') return this.#canPreview ? { state: 'idle' } : { state: 'unknown', reason: this.#t('notSimulated') };
    if (this.#unsimulated) {
      const reason = p?.code === 'error' ? this.#t('previewFailed', { error: esc(p.params?.error ?? '') }) : this.#coded('notices', p?.code, p?.params, p?.notice ?? this.#t('previewUnavailable'));
      return { state: 'unknown', reason: `${reason} ${this.#t('treeStillExact')}` };
    }
    const reason = (v) => (v.reasonCode || v.reason ? this.#coded('reasons', v.reasonCode, v.params, v.reason) : '');
    const verdicts = p.verdicts ?? [];
    const refused = verdicts.find((v) => v.allowed === false);
    if (refused) return { state: 'refused', reason: reason(refused) || this.#t('contractError', { code: esc(refused.code ?? '?') }) };
    const unknown = verdicts.find((v) => v.allowed == null);
    if (p.outcome && !p.outcome.ok && !unknown) {
      const codes = (p.outcome.codes ?? []).map((c) => esc(c.code ?? c)).join(', ');
      return { state: 'refused', reason: this.#t('wouldFail', { error: esc(p.outcome.error ?? 'unknown error'), codes: codes || null }) };
    }
    if (unknown) return { state: 'unknown', reason: reason(unknown) };
    const accepted = verdicts.find((v) => v.allowed);
    return { state: 'success', reason: (accepted && reason(accepted)) || this.#t('simulatedOk') };
  }

  #statusHtml() {
    const o = this.#outcome();
    const icon = { success: '✓', refused: '✕', unknown: '?', idle: '•' }[o.state];
    const title = this.#t({ success: 'statusSuccess', refused: 'statusRefused', unknown: 'statusUnknown', checking: 'statusChecking', idle: 'statusIdle' }[o.state]);
    const mark = o.state === 'checking' ? '<span class="spinner" aria-hidden="true"></span>' : `<span class="icon" aria-hidden="true">${icon}</span>`;
    const button = o.state === 'idle' ? `<button type="button" data-act="preview" part="preview-button">${this.#t('previewButton')}</button>` : '';
    return `<section id="ssd-status" class="status" part="status" data-state="${o.state}" role="status" aria-live="polite">${mark}
      <div><b>${title}</b>${o.reason ? `<div class="small" part="status-reason">${o.reason}</div>` : ''}${button}</div></section>`;
  }

  #payment(p) {
    return { amount: this.#amount(p.amount, p.token), to: this.#addr(p.to) };
  }

  /** A one-line, plain account of one authorized call. */
  #summaryLine(fn) {
    const p = paymentOf(fn, this.signer, (c) => this.#token(c));
    return p ? this.#t('summarySend', this.#payment(p)) : this.#sentence(fn);
  }

  /** The call, the signer's payments in its sub-calls, and a count of the rest, which Details lists. */
  #summaryEntry(root) {
    const s = subCallSummary(root, this.signer, (c) => this.#token(c));
    return this.#t('summaryLine', {
      lead: this.#summaryLine(root.fn),
      payments: s.payments.map((p) => this.#t('summaryPayment', this.#payment(p))),
      more: s.more ? `<button type="button" class="more-calls" data-act="details" part="summary-more">${this.#t('summaryMore', { count: s.more })}</button>` : null,
    });
  }

  #summaryHtml() {
    const op = this.#decoded.op;
    const cmp = this.#comparison;
    const lines = this.#entries.length
      ? this.#entries.map((e, i) => `<li>${this.#summaryEntry(e.root)}${cmp?.entries[i]?.status === 'extra' ? ` <span class="badge bad">${this.#t('summaryUnusedTag')}</span>` : ''}</li>`)
      : op?.type === 'invoke' ? [`<li>${this.#summaryLine(op.hostFunction)}</li>`] : [`<li>${this.#t('operation', { type: esc(op?.type ?? 'unknown') })}</li>`];
    const budgets = (this.#previewState === 'done' ? this.#preview.budgets ?? [] : []).map((b) => {
      const tok = this.#assetToken(b.token);
      const limit = BigInt(b.limit), remaining = limit - BigInt(b.spentInWindow), amount = BigInt(b.amount ?? 0);
      const over = amount > remaining;
      const left = over ? (remaining > 0n ? remaining : 0n) : remaining - amount;
      return `<p class="budget-line" part="budget-line" data-over="${over}">${this.#t('summaryBudget', { left: esc(formatAmount(left, tok.decimals)), limit: esc(formatAmount(limit, tok.decimals)), code: esc(tok.code), over })}</p>`;
    }).join('');
    return `<section id="ssd-what" class="what" part="what"><ul>${lines.join('')}</ul>${budgets}</section>`;
  }

  /** Plain warnings from the independent recording. */
  #warningsHtml() {
    const cmp = this.#comparison;
    const out = this.#flags().some((f) => f.kind === 'not-simulated') ? [this.#t('notSimulatedWarning')] : [];
    for (const e of cmp?.entries ?? []) {
      if (e.status === 'extra') out.push(this.#t('unusedWarning', { what: this.#summaryLine(this.#entries[e.index].root.fn) }));
      if (e.status === 'differs') out.push(this.#t('differsWarning'));
    }
    if (cmp?.missing.length) out.push(this.#t('missingWarning', { count: cmp.missing.length }));
    return out.map((w) => `<div class="notice bad" part="warning" role="alert">${w}</div>`).join('');
  }

  #budget(b) {
    const tok = this.#assetToken(b.token);
    const limit = BigInt(b.limit), spent = BigInt(b.spentInWindow), amount = BigInt(b.amount ?? 0);
    const remaining = limit - spent;
    const fits = amount <= remaining;
    const pct = (x) => (limit > 0n ? Math.max(0, Math.min(100, Number((x * 10000n) / limit) / 100)) : 0);
    const f = (x) => this.#amount(x, tok);
    const minutes = b.nextReleaseLedger && b.ledger ? Math.round(((b.nextReleaseLedger - b.ledger) * 5) / 60) : null;
    return `<div class="card" part="budget"><b>${this.#t('spendingCap')}</b>${b.rule ? this.#t('capRule', { rule: esc(b.rule.name ?? b.rule.id) }) : ''}
      <div class="meter" role="img" aria-label="${esc(this.#t('capMeter', { spent: f(spent), limit: f(limit), amount: f(amount) }).replace(/<[^>]*>/g, ''))}">
        <span class="spent" data-pct="${pct(spent)}"></span><span class="${fits ? 'this' : 'over'}" data-pct="${pct(fits ? amount : remaining > 0n ? remaining : 0n)}"></span></div>
      <div class="small">${this.#t('capLeft', { remaining: f(remaining), limit: f(limit), period: esc(b.periodLedgers) })}
      ${fits ? this.#t('capAfter', { after: f(remaining - amount) }) : this.#t('capOver', { amount: f(amount) })}
      ${b.nextReleaseLedger ? `<span class="muted">${this.#t('capRelease', { ledger: esc(b.nextReleaseLedger), minutes })}</span>` : ''}</div></div>`;
  }

  #event(ev) {
    const [name, ...rest] = ev.topics;
    const token = ev.contract ? this.#token(ev.contract.str) : null;
    const where = `<span class="muted small">(${esc(this.#label(ev.contract?.str) ?? short(ev.contract?.str ?? ''))})</span>`;
    if (name?.t === 'symbol' && name.v === 'transfer' && rest.length >= 2) {
      const amount = ev.data.t === 'map' ? ev.data.v.find((e) => e.k.v === 'amount')?.v : ev.data;
      const amt = amount && token && typeof amount.v === 'bigint' ? this.#amount(amount.v, token) : esc(amount?.v?.toString() ?? '?');
      return `${this.#t('transferEvent', { amount: amt, from: this.#val(rest[0]), to: this.#val(rest[1]) })} ${where}`;
    }
    return `<code>${esc(name?.v ?? name?.t ?? '?')}</code> ${rest.map((t) => this.#val(t)).join(' ')} → ${this.#val(ev.data)} ${where}`;
  }

  #render() {
    const d = this.dialog;
    if (!d) return;
    const focused = this.#root.activeElement?.closest?.('[data-act]')?.dataset.act;
    // innerHTML replaces the disclosure; keep it as the user left it.
    const more = d.querySelector('details[part="details"]');
    if (more) this.#detailsOpen = more.open;
    const dapp = this.getAttribute('dapp');
    const netKey = Object.entries(NETWORKS).find(([, v]) => v === this.networkPassphrase)?.[0];
    const net = netKey ? this.#strings.networks[netKey] ?? netKey : this.#t('customNetwork');
    let body, title, sub = '';
    if (this.#error) {
      title = this.#t('titleUnreadable');
      body = `<div class="notice bad" role="alert">${this.#t('unreadable', { error: esc(this.#error.message) })}</div>`;
    } else if (!this.#decoded) {
      title = this.#t('titleEmpty');
      body = `<p class="muted">${this.#t('nothingToReview')}</p>`;
    } else {
      const n = this.#entries.length;
      const op = this.#decoded.op;
      title = n === 1 ? this.#t('titleOne') : n ? this.#t('titleMany', { count: n }) : this.#t('titleTransaction');
      const tx = this.#decoded.envelope?.tx;
      if (tx) sub = this.#t('summary', { fee: esc(formatAmount(BigInt(tx.fee), 7)), payer: this.#addr(tx.source) });
      const entries = n ? this.#entries.map((e, i) => this.#entry(e, i)).join('')
        : op?.type === 'invoke' ? `<article class="entry" part="entry"><div class="entry-head"><span>${this.#t('sourceAuthority')}</span></div><ol class="tree" role="list">${this.#node({ fn: op.hostFunction, subs: [] })}</ol></article>`
        : `<p>${this.#t('operation', { type: esc(op?.type ?? 'unknown') })}</p>`;
      const open = this.#detailsOpen ?? this.getAttribute('details') === 'open';
      body = `${this.#summaryHtml()}${this.#statusHtml()}${this.#warningsHtml()}
        <details class="more" part="details"${open ? ' open' : ''}><summary part="details-summary">${this.#t('details')}</summary>
        <h3>${this.#t('whatYouAuthorize')}</h3><div part="tree">${entries}</div>${this.#effectsHtml()}</details>`;
    }
    const flags = this.#flags();
    const risky = flags.length > 0 || !!this.#error;
    const strict = this.hasAttribute('strict');
    const checking = this.#previewState === 'loading';
    const approveLabel = this.#error ? this.#t('approveButton') : this.#confirming ? this.#t('confirmApproveAnyway') : risky ? this.#t('approveAnyway') : this.#t('approveButton');
    d.innerHTML = `<header part="header">
        <div class="meta"><slot name="logo"></slot><span class="chip" part="network">${esc(net)}</span>${dapp ? `<span part="dapp">${this.#t('requestFrom', { dapp: esc(dapp) })}</span>` : ''}</div>
        <h2 id="ssd-title" part="title">${title}</h2>
        <p id="ssd-sub" class="sub" part="fee">${sub}</p>
        <slot name="header"></slot></header>
      <div class="body" part="body">${body}</div>
      <footer part="footer">
        <div class="footer-slot"><slot name="footer"></slot></div>
        <div class="actions">
          <button type="button" data-act="reject" part="reject">${this.#t('reject')}</button>
          <button type="button" data-act="approve" part="approve" class="${risky ? 'danger' : 'primary'}" ${this.#error || (strict && risky) || checking || !this.#decoded ? 'disabled' : ''}>${approveLabel}</button>
        </div>
      </footer>`;
    // Widths through the CSSOM: no style attributes, so no `unsafe-inline`.
    for (const el of d.querySelectorAll('[data-pct]')) el.style.width = `${el.dataset.pct}%`;
    if (focused && d.open) (d.querySelector(`[data-act="${focused}"]:not([disabled])`) ?? d.querySelector('[data-act="reject"]'))?.focus();
  }

  #onClick(e) {
    const act = e.target.closest?.('[data-act]')?.dataset.act;
    if (act === 'reject') this.#finish('rejected');
    else if (act === 'preview') this.runPreview();
    else if (act === 'details') {
      const more = this.dialog.querySelector('details[part="details"]');
      if (!more) return;
      more.open = true;
      this.#detailsOpen = true;
      more.querySelector('summary')?.focus();
      more.scrollIntoView?.({ block: 'nearest' });
    } else if (act === 'approve') {
      if (this.#flags().length && !this.#confirming) {
        this.#confirming = true;
        this.#render();
        this.#root.querySelector('[data-act="approve"]')?.focus();
        return;
      }
      this.#approve();
    }
  }

  /**
   * The approval payload minus flags and preview, computed when the request is
   * decoded. Hashing is async, and a wallet that opens a passkey prompt from
   * `approve` needs the event inside the click's user activation.
   */
  async #approvalDetail() {
    const passphrase = this.networkPassphrase;
    const signer = this.signer;
    const entries = await Promise.all(this.#entries.map(async (e, index) => ({
      index,
      address: e.credentials.address?.str ?? null,
      credentials: e.credentials.type,
      nonce: e.credentials.nonce?.toString() ?? null,
      entryXdr: toB64(e.raw),
      invocationXdr: toB64(e.root.raw),
      invocationHash: hex(await sha256(e.root.raw)),
      forSigner: !!signer && e.credentials.address?.str === signer,
    })));
    const tx = this.#decoded?.envelope;
    return {
      transaction: this.transaction ?? null,
      transactionHash: tx ? hex(await transactionHash(tx, passphrase)) : null,
      networkPassphrase: passphrase,
      signer,
      entries,
    };
  }

  async #approve() {
    const pending = this.#approval;
    let base;
    try {
      base = await pending;
    } catch (err) {
      this.#error = err;
      this.#render();
      return;
    }
    if (!base || pending !== this.#approval) return;
    const detail = { ...base, flags: this.#flags(), preview: this.#preview };
    const ok = this.dispatchEvent(new CustomEvent('approve', { bubbles: true, composed: true, cancelable: true, detail }));
    if (!ok) return;
    this.#close();
    this.#pending?.resolve({ approved: true, ...detail });
    this.#pending = null;
  }

  #finish(reason) {
    if (!this.dialog.open && !this.#pending) return;
    this.dispatchEvent(new CustomEvent('reject', { bubbles: true, composed: true, detail: { reason, flags: this.#flags() } }));
    this.#close();
    this.#pending?.resolve({ approved: false, reason });
    this.#pending = null;
  }

  #close() {
    this.#abort?.abort();
    if (this.dialog.open) this.dialog.close();
    this.removeAttribute('open');
    this.dispatchEvent(new CustomEvent('close', { bubbles: true, composed: true }));
  }

  #trapFocus(e) {
    if (e.key !== 'Tab' || this.hasAttribute('inline')) return;
    const items = [...this.dialog.querySelectorAll('button:not([disabled]), summary, [tabindex]:not([tabindex="-1"])')].filter((x) => x.offsetParent !== null);
    if (!items.length) return;
    const first = items[0], last = items[items.length - 1];
    const active = this.#root.activeElement;
    if (e.shiftKey && active === first) { e.preventDefault(); last.focus(); }
    else if (!e.shiftKey && active === last) { e.preventDefault(); first.focus(); }
  }
}

/**
 * Register the element. Safe to call more than once, and a no-op where
 * custom elements do not exist (server rendering).
 */
export function define(tag = TAG) {
  const registry = globalThis.customElements;
  if (!registry) return;
  if (!registry.get(tag)) registry.define(tag, tag === TAG ? SorobanSignDialog : class extends SorobanSignDialog {});
}

/** Open a shared dialog for one request: the one-call embed. */
export function request(opts = {}) {
  define();
  let el = document.querySelector(`${TAG}[data-shared]`);
  if (!el) {
    el = document.createElement(TAG);
    el.dataset.shared = '';
    document.body.append(el);
  }
  return el.request(opts);
}
