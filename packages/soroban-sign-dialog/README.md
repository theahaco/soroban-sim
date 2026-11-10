# @theahaco/soroban-sign-dialog

A signing dialog for Soroban that a wallet or dapp drops into its own page.
It shows the user exactly what they are about to authorize, decoded from the
transaction's XDR, and can preview the effects in a local simulator before
anything is signed.

- **The authorization tree, always.** Every entry, every nested call, with
  argument values and readable sentences for token transfers. It is decoded
  in the browser from the bytes the signature covers. No network, no
  simulator, any protocol. About 17 KB brotli.
- **A simulated outcome.** When the wallet gives it an engine, the dialog
  lazily loads a WebAssembly build of soroban-sim's host (about 470 KB
  brotli) as it opens. It runs the transaction against current ledger state,
  records which authorizations the transaction really needs, and reports one
  status: will succeed, will be refused, or unknown, with a one-line reason.
  Balance changes, events and a smart account's own verdict and spending cap
  come with it.
- **It never holds keys.** Approval hands back the exact entries to sign and
  a hash of each reviewed invocation. The wallet signs.
- **White-label.** No product names in the UI. Theme tokens, `::part()`
  hooks, slots for your logo, header and footer, and every string
  replaceable.
- **Works anywhere.** A standard custom element with no dependencies. Safe to
  import during server rendering, runs under a strict Content-Security-Policy,
  tested with plain HTML, React 18 and 19, Vue 3, Svelte 5 and Angular.

## Quick start

One line in the page, one call when the user wants to approve:

```html
<script type="module" src="/vendor/soroban-sign-dialog/soroban-sign-dialog.js"></script>
<script type="module">
  const r = await SorobanSignDialog.request({
    transaction,               // base64 TransactionEnvelope, as built by your dapp
    network: 'testnet',        // or 'mainnet', or a full passphrase
    signer: account,           // the address the wallet signs for
    dapp: location.host,
  });
  if (r.approved) {
    // r.entries: every authorization entry, with entryXdr, invocationHash and
    // forSigner. Sign the ones that are yours, exactly as given.
    await wallet.signEntries(r.entries.filter((e) => e.forSigner));
  }
</script>
```

`request()` creates a shared dialog on first use and resolves with
`{ approved: true, ...detail }` or `{ approved: false, reason }`.

Serve the files from your own origin: copy `dist/` (it holds precompressed
`.br` siblings and `SHA256SUMS`). Nothing is fetched from a CDN.

### From a bundler

```js
import { request } from '@theahaco/soroban-sign-dialog';

const r = await request({ transaction, signer, network: 'mainnet' });
```

The package's main module has no side effects. `request()` registers the
element on first use; call `define()` yourself if you put
`<soroban-sign-dialog>` in markup.

### As markup

```html
<soroban-sign-dialog id="sign" network="testnet" signer="C…">
  <img slot="logo" src="/logo.svg" alt="Acme Wallet" height="20">
</soroban-sign-dialog>
<script type="module">
  import { define } from '@theahaco/soroban-sign-dialog';
  define();
  const dialog = document.getElementById('sign');
  dialog.labels = addressBook;              // rich data: properties
  dialog.transaction = transactionXdr;
  dialog.addEventListener('approve', (e) => wallet.signEntries(e.detail.entries));
  dialog.addEventListener('reject', (e) => console.log(e.detail.reason));
  dialog.show();
</script>
```

## Frameworks

The element takes strings as attributes and rich data (`labels`, `strings`,
`previewProvider`) as properties, and reports with standard `CustomEvent`s.
Each snippet below is a trimmed copy of a working example in `examples/`,
which the browser tests mount and drive to an approve and a reject.

### React 18 and 19

```jsx
import { SignDialog } from '@theahaco/soroban-sign-dialog/react';

<SignDialog
  open={open}
  transaction={tx}
  network="testnet"
  signer={account}
  labels={addressBook}
  onApprove={(e) => sign(e.detail.entries)}
  onReject={(e) => setOpen(false)}
>
  <img slot="logo" src="/logo.svg" alt="Acme Wallet" />
</SignDialog>
```

React 18 cannot pass objects or listen for custom events on a custom element
by itself; the wrapper sets properties through a ref and adds the listeners,
so 18 and 19 behave the same. The ref is the element. It renders on the
server as a plain `<soroban-sign-dialog>` tag.

### Vue 3

```js
// main.js
import { define } from '@theahaco/soroban-sign-dialog';
define(); // before mounting, so Vue sets `labels` as a property
app.config.compilerOptions.isCustomElement = (tag) => tag === 'soroban-sign-dialog';
```

```vue
<soroban-sign-dialog ref="dialog" :transaction="tx" network="testnet" :signer="account"
  :labels="addressBook" @approve="(e) => sign(e.detail.entries)" @reject="onReject" />
```

With Vite, put `isCustomElement` under `vue({ template: { compilerOptions } })`
instead. For typed templates add
`/// <reference types="@theahaco/soroban-sign-dialog/vue" />`.

### Svelte 5

```svelte
<script>
  import { define } from '@theahaco/soroban-sign-dialog';
  define(); // before the component mounts
  let dialog;
</script>

<soroban-sign-dialog bind:this={dialog} transaction={tx} network="testnet" signer={account}
  labels={addressBook} onapprove={(e) => sign(e.detail.entries)} onreject={onReject} />
```

Typings: `/// <reference types="@theahaco/soroban-sign-dialog/svelte" />`.

### Angular

```ts
import { CUSTOM_ELEMENTS_SCHEMA, Component } from '@angular/core';
import { define, type ApproveDetail } from '@theahaco/soroban-sign-dialog';
define();

@Component({
  selector: 'app-sign',
  schemas: [CUSTOM_ELEMENTS_SCHEMA],
  template: `<soroban-sign-dialog #dialog [transaction]="tx" network="testnet" [signer]="account"
    [labels]="addressBook" (approve)="onApprove($event)" (reject)="onReject($event)" />`,
})
export class SignComponent {
  onApprove(e: Event) { this.sign((e as CustomEvent<ApproveDetail>).detail.entries); }
}
```

Works zoneless or with zone.js; the example runs zoneless.

### Plain HTML

See [Quick start](#quick-start) and `examples/html/`.

## What the user sees

Top to bottom, by default:

1. **A summary** of what is being approved, one plain line per authorized
   call ("Send 2 XLM to Corner Café"), and, when the preview finds a spending
   cap, how much of it is left ("5 of 10 XLM left this window after this").
   A line also names the payments of known tokens that the signer makes in
   the call's sub-calls ("Call swap on Router, paying 2 XLM to Pool"), up to
   three, and counts the other sub-calls ("and 2 more calls"); the count
   opens Details, which lists them all.
2. **One status** for the simulated outcome: *Will succeed*, *Will be
   refused* or *Outcome unknown*, with a one-line reason. It runs as the dialog
   opens when an engine or provider is set. On a protocol the engine has no
   build for, or with no engine, the status is unknown and says why; the
   summary and the tree still come from the request's own bytes.
3. **Plain warnings**, such as an authorization the transaction does not use.
4. **Details**, one disclosure holding the full authorization tree with raw
   contract, function and argument data, the account's verdict and cap, balance
   changes and events.
5. **Reject** and **Approve**. Approve is disabled while the outcome is being
   checked. While a warning or refusal stands, or a started preview could not
   simulate the request, Approve reads *Approve anyway* and needs a second
   click (`strict` disables it instead).

`details="open"` starts with Details expanded, for a wallet's own signing page.

## Make it yours

### Theme tokens

Set `--ssd-*` custom properties on the element (or any ancestor):

| Group | Tokens |
|---|---|
| Colour | `--ssd-bg`, `--ssd-surface`, `--ssd-fg`, `--ssd-muted`, `--ssd-border`, `--ssd-accent`, `--ssd-accent-fg`, `--ssd-ok`, `--ssd-ok-bg`, `--ssd-warn`, `--ssd-warn-bg`, `--ssd-danger`, `--ssd-danger-bg`, `--ssd-danger-fg`, `--ssd-backdrop` |
| Typography | `--ssd-font`, `--ssd-mono`, `--ssd-font-size`, `--ssd-line-height`, `--ssd-title-size`, `--ssd-small-size`, `--ssd-weight-strong` |
| Shape | `--ssd-radius`, `--ssd-radius-sm`, `--ssd-radius-pill`, `--ssd-border-width`, `--ssd-width` |
| Spacing | `--ssd-space-1` … `--ssd-space-4` |
| Elevation | `--ssd-shadow` |
| Motion | `--ssd-duration`, `--ssd-easing` (animations stop under `prefers-reduced-motion`) |

```css
soroban-sign-dialog {
  --ssd-accent: #0f766e;
  --ssd-radius: 20px;
  --ssd-font: "Inter", system-ui, sans-serif;
}
soroban-sign-dialog::part(network) { text-transform: uppercase; }
```

Light and dark follow `prefers-color-scheme`; `theme="light"` or
`theme="dark"` pins one. Dark values are the same tokens, so override them
inside your own dark-mode rule.

Parts: `dialog`, `header`, `network`, `dapp`, `title`, `fee`, `body`, `what`,
`summary-more`, `budget-line`, `status`, `status-reason`, `warning`, `details`, `details-summary`,
`tree`, `entry`, `unused-entry`, `node`, `node-details`, `doc`, `preview`,
`preview-button`, `auth-check`, `verdict`, `budget`, `balances`, `events`,
`notice`, `footer`, `approve`, `reject`.

### Slots

| Slot | Where |
|---|---|
| `logo` | Start of the header row, before the network chip |
| `header` | Under the title and fee line |
| `footer` | Left of the Reject and Approve buttons |

### Strings and languages

Every word the dialog shows comes from `dialog.strings`, English by default.
Assign any subset; the rest keep their defaults.

```js
dialog.strings = {
  approveButton: 'Aprobar',
  reject: 'Rechazar',
  details: 'Detalles',
  titleOne: 'Autoriza esta llamada',
  summarySend: (p) => `Enviar ${p.amount} a ${p.to}`,
  statusSuccess: 'Saldrá bien',
  notices: { protocol: (p) => `La red usa el protocolo ${p.protocol}.` },
};
```

Values are inserted as HTML. Parameters arrive already escaped (addresses and
amounts as small pieces of markup), so a translation only escapes text of its
own. The full list, with every parameter, is `src/strings.js` (exported as
`STRINGS`). The engine reports notices and verdict reasons as codes with
parameters (`notices`, `reasons`), so those translate too.

## The preview

Opt in by giving the dialog a preview provider. The engine is a separate
module, imported when the dialog first opens with one (or when the user asks,
with `preview="manual"`):

```js
import { createPreview } from '@theahaco/soroban-sign-dialog/preview';

dialog.previewProvider = createPreview({
  rpcUrl: 'https://your-rpc.example',  // the wallet's RPC, not the dapp's
  tokens: { 'C…': { code: 'USDC', decimals: 7 } },
});
```

Or point the `engine` attribute at `dist/preview/soroban-sim-preview.js` and
set `rpc-url`. `preview="manual"` waits for the user to press "Check outcome";
`preview="off"` never simulates.

What the preview does:

1. Reads the latest ledger and, in one parallel `getLedgerEntries` batch, the
   network config, the transaction's footprint and the nonces of its entries.
2. Picks the reactor build for the network's `protocolVersion` and downloads
   only that one. Builds exist for protocols 29 (mainnet today) and 30
   (testnet today). On any other protocol it shows the tree with a notice and
   simulates nothing, because a simulation under other rules can be wrong.
3. Records the transaction's authorizations on its own, the way RPC's
   `simulateTransaction` does, and compares them with the request.
4. Reports balance changes, events and resource use.

### Entries a transaction does not use

A Soroban transaction may carry signed authorization entries that none of
its calls consume. The network accepts the transaction anyway, and such an
entry's signature stays valid until it expires, so it can be submitted in a
later transaction. The tree alone cannot tell such an entry apart from one the
transaction needs. The preview can: an entry the independent recording does
not contain is tagged "not used" in the summary, a plain warning above the
buttons says what it authorizes and why that matters, Approve turns into
**Approve anyway** and needs a second click, and the `approve` detail carries
an `extra` flag. With `strict`, Approve stays disabled instead.

Other flags: `differs` (same signer, different tree), `missing` (the
recording needs an entry the request lacks), `refused` (the account would
refuse), `fails` (the transaction would fail), `not-simulated` (a preview
ran but ended in an error or the tree-only fallback; the warning reads "This
request could not be simulated. Approve anyway?").

### Smart-account verdicts (local fork only)

Recording mode never runs a contract account's `__check_auth`, so it cannot
say whether the account will accept. To answer that before the user signs,
the engine runs a second, enforcing simulation for each account the wallet
describes with a hint:

```js
createPreview({
  rpcUrl,
  hints: {
    [account]: {
      verifier: 'C…',          // the verifier contract that checks this key
      key: '657fb0df…',        // the key's public bytes, hex
      grantId: 1, ruleId: 1,   // optional: prefetch the grant and spend window
      ruleIds: [1],            // or name the rule each authorized call uses
    },
  },
});
```

For that run only, inside the reactor's in-memory copy of the ledger, the
engine:

- replaces the verifier contract's code with a 397-byte stub whose `verify`
  returns true (source: `fixtures/contracts/preview-stub-verifier`);
- fills the entry's credential with the payload the account reads,
  `{ context_rule_ids, signers: { External(verifier, key) => 64 zero bytes } }`.

The account's own code then checks that the key belongs to the selected rule,
calls the stub, and applies its rules and spending window as it would on
chain. The dialog reports "would be accepted" or "would be refused", meaning:
*assuming a valid signature from this key*. A refusal counts only when the
account's own contract raised the error. When the request also needs other
parties who have not signed yet, the verdict is "unknown" and the balances
and events come from the recording. Nothing is signed, nothing is
written anywhere but the reactor's memory, and the stub never leaves it.

This works for any account that checks an External signer through a
verifier contract's `verify`: the External-signer scheme Perch and
OpenZeppelin's smart accounts share. The spending-cap card reads the grant layout of the Perch
release in the test fixture; on other layouts the verdict still shows and the
cap card does not. Re-simulate after signing, as wallets already do, to check
the real signature.

### What the preview can and cannot know

- It trusts the RPC for every ledger entry: Stellar RPC returns entries
  without proofs. Use the wallet's own RPC.
- State moves on between preview and inclusion; the result names the ledger
  it ran at.
- Fees: the dialog shows the fee the envelope declares.

## Security model

The dialog shows what the bytes say; it cannot protect a user from the page
it runs in. A page that embeds it can also skip it. Its guarantees are
strongest inside a wallet's own signing page (`inline` renders it in place),
where the wallet controls the code. Embedded by a dapp, it is a clear review
step for honest dapps and a check against transactions that a backend or a
dependency built wrongly.

The `approve` detail binds the review to what gets signed: each entry's
`invocationHash` is the sha256 of the invocation the user saw. Check it
against what you sign.

## Content-Security-Policy

The tree view needs nothing: styles use constructable stylesheets (no
`style-src 'unsafe-inline'`), and there is no `eval` or inline script. Browsers
without constructable stylesheets (Safari before 16.4) get a `<style>` element,
which a policy without `'unsafe-inline'` blocks.

The preview needs:

```
script-src 'self' 'wasm-unsafe-eval';
worker-src 'self';
connect-src 'self' https://your-rpc.example;
```

The demo runs under `default-src 'self'; script-src 'self' 'wasm-unsafe-eval';
style-src 'self'; worker-src 'self'`, and its browser test fails on any CSP
violation.

## Sizes

CI fails the build when a budget is exceeded (`pnpm size`):

| Artifact | brotli | Budget |
|---|---:|---:|
| Tree view (`soroban-sign-dialog.js`, the whole core) | ~17 KB | 20 KB |
| Engine, protocol 29 (engine JS + worker + reactor) | ~476 KB | 600 KB |
| Engine, protocol 30 | ~488 KB | 600 KB |
| React wrapper (beside the core) | ~0.5 KB | 2 KB |

The reactor is soroban-env-host and its Wasm interpreter compiled for
`wasm32-wasip1`; it is cached after the first preview. Serve the `.br` files
with `Content-Encoding: br` and `.wasm` as `application/wasm`.

## Developing

```sh
pnpm install
pnpm --filter @theahaco/soroban-sign-dialog build      # cargo + wasm-opt + esbuild
pnpm --filter @theahaco/soroban-sign-dialog test       # decoder, comparison, engine, SSR
pnpm --filter @theahaco/soroban-sign-dialog test:browser
pnpm --filter @theahaco/soroban-sign-dialog demo       # http://127.0.0.1:4178/demo/
```

The build needs Rust with the `wasm32-wasip1` target, and `wasm-opt`
(binaryen) on `PATH` or in `WASM_OPT` for the published sizes. The reactor
crate is `crates/soroban-sim-preview`. Its tests and the engine test replay
the same captured ledger: `crates/soroban-sim-preview/fixtures/perch-capture.json`.

API reference: [API.md](API.md).
