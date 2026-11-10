// The dialog's stylesheet. Every visual decision reads a `--ssd-*` custom
// property, so a wallet restyles the dialog from its own CSS without
// touching the shadow root; `::part()` reaches the rest.

const DARK = `
  --ssd-bg: #17191e; --ssd-surface: #20232a; --ssd-fg: #eceef2; --ssd-muted: #a3aab6;
  --ssd-border: #33373f; --ssd-accent: #7f9cff; --ssd-accent-fg: #0d1020;
  --ssd-ok: #6fd39a; --ssd-ok-bg: #173323; --ssd-warn: #f2c063; --ssd-warn-bg: #3a2c10;
  --ssd-danger: #ff8a7e; --ssd-danger-bg: #3d1c19; --ssd-danger-fg: #1a0705;
  --ssd-backdrop: rgba(0, 0, 0, 0.65); --ssd-shadow: 0 18px 50px rgba(0, 0, 0, 0.6);
  color-scheme: dark;`;

export const STYLE = `
:host {
  /* Colour */
  --ssd-bg: #ffffff;
  --ssd-surface: #f5f6f8;
  --ssd-fg: #16181d;
  --ssd-muted: #5d6470;
  --ssd-border: #dde1e7;
  --ssd-accent: #2f5bd8;
  --ssd-accent-fg: #ffffff;
  --ssd-ok: #1d7a46;
  --ssd-ok-bg: #e5f4ea;
  --ssd-warn: #8a5a00;
  --ssd-warn-bg: #fdf1d8;
  --ssd-danger: #b42318;
  --ssd-danger-bg: #fde8e6;
  --ssd-danger-fg: #ffffff;
  --ssd-backdrop: rgba(10, 12, 16, 0.55);
  /* Typography */
  --ssd-font: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  --ssd-mono: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
  --ssd-font-size: 15px;
  --ssd-line-height: 1.45;
  --ssd-title-size: 1.12rem;
  --ssd-small-size: 0.84rem;
  --ssd-weight-strong: 600;
  /* Shape */
  --ssd-radius: 12px;
  --ssd-radius-sm: 8px;
  --ssd-radius-pill: 999px;
  --ssd-border-width: 1px;
  --ssd-width: 34rem;
  /* Spacing */
  --ssd-space-1: 0.25rem;
  --ssd-space-2: 0.5rem;
  --ssd-space-3: 0.75rem;
  --ssd-space-4: 1.25rem;
  /* Elevation */
  --ssd-shadow: 0 18px 50px rgba(0, 0, 0, 0.25);
  /* Motion */
  --ssd-duration: 160ms;
  --ssd-easing: cubic-bezier(0.2, 0, 0, 1);
  display: contents;
  color-scheme: light;
}
@media (prefers-color-scheme: dark) {
  :host(:not([theme="light"])) {${DARK}
  }
}
:host([theme="dark"]) {${DARK}
}
dialog {
  box-sizing: border-box; width: min(var(--ssd-width), calc(100vw - 2rem)); max-height: min(46rem, calc(100dvh - 2rem));
  padding: 0; border: var(--ssd-border-width) solid var(--ssd-border); border-radius: var(--ssd-radius);
  background: var(--ssd-bg); color: var(--ssd-fg); font-family: var(--ssd-font); font-size: var(--ssd-font-size);
  line-height: var(--ssd-line-height); box-shadow: var(--ssd-shadow); overflow: hidden;
}
dialog[open] { display: flex; flex-direction: column; animation: ssd-in var(--ssd-duration) var(--ssd-easing); }
dialog::backdrop { background: var(--ssd-backdrop); }
@keyframes ssd-in { from { opacity: 0; transform: translateY(0.5rem); } }
:host([inline]) dialog { position: static; width: 100%; max-height: none; box-shadow: none; margin: 0; animation: none; }
@media (max-width: 560px) {
  :host(:not([inline])) dialog {
    width: 100vw; max-width: 100vw; max-height: 92dvh; margin: auto 0 0; border-radius: var(--ssd-radius) var(--ssd-radius) 0 0;
  }
}
header { padding: calc(var(--ssd-space-4) - var(--ssd-space-1)) var(--ssd-space-4) var(--ssd-space-3); border-bottom: var(--ssd-border-width) solid var(--ssd-border); }
.meta { display: flex; flex-wrap: wrap; gap: var(--ssd-space-2); align-items: center; color: var(--ssd-muted); font-size: var(--ssd-small-size); }
::slotted([slot="logo"]) { max-height: 1.5rem; }
.chip { border: var(--ssd-border-width) solid var(--ssd-border); border-radius: var(--ssd-radius-pill); padding: 0 var(--ssd-space-2); background: var(--ssd-surface); }
h2 { margin: var(--ssd-space-2) 0 var(--ssd-space-1); font-size: var(--ssd-title-size); line-height: 1.3; font-weight: var(--ssd-weight-strong); }
.sub { margin: 0; color: var(--ssd-muted); font-size: var(--ssd-small-size); }
.body { overflow: auto; padding: var(--ssd-space-3) var(--ssd-space-4) var(--ssd-space-4); flex: 1 1 auto; min-height: 0; }
h3 { font-size: 0.78rem; letter-spacing: 0.04em; text-transform: uppercase; color: var(--ssd-muted); margin: var(--ssd-space-4) 0 var(--ssd-space-2); }
h3:first-child { margin-top: var(--ssd-space-2); }
b { font-weight: var(--ssd-weight-strong); }
.what ul { list-style: none; margin: var(--ssd-space-2) 0; padding: 0; }
.what li { font-size: var(--ssd-title-size); font-weight: var(--ssd-weight-strong); line-height: 1.35; overflow-wrap: anywhere; margin: 0 0 var(--ssd-space-1); }
.what .more-calls { font-weight: inherit; min-height: 0; padding: 0; border: 0; background: none; color: var(--ssd-accent); text-decoration: underline; }
.budget-line { margin: 0 0 var(--ssd-space-2); color: var(--ssd-muted); font-size: var(--ssd-small-size); }
.budget-line[data-over="true"] { color: var(--ssd-danger); font-weight: var(--ssd-weight-strong); }
.status { display: flex; gap: var(--ssd-space-2); align-items: flex-start; border: var(--ssd-border-width) solid var(--ssd-border); border-radius: var(--ssd-radius-sm); padding: var(--ssd-space-2) var(--ssd-space-3); margin: var(--ssd-space-2) 0; background: var(--ssd-surface); }
.status .icon { font-weight: 700; font-size: 1.1rem; line-height: 1.2; color: var(--ssd-muted); }
.status[data-state="success"] { background: var(--ssd-ok-bg); border-color: color-mix(in srgb, var(--ssd-ok) 45%, transparent); }
.status[data-state="success"] .icon { color: var(--ssd-ok); }
.status[data-state="refused"] { background: var(--ssd-danger-bg); border-color: color-mix(in srgb, var(--ssd-danger) 45%, transparent); }
.status[data-state="refused"] .icon { color: var(--ssd-danger); }
.status .spinner { margin-top: 0.25em; }
.status button { margin-top: var(--ssd-space-2); }
details.more { margin-top: var(--ssd-space-3); border-top: var(--ssd-border-width) solid var(--ssd-border); padding-top: var(--ssd-space-2); }
details.more > summary { font-size: var(--ssd-font-size); font-weight: var(--ssd-weight-strong); color: var(--ssd-fg); padding: var(--ssd-space-1) 0; }
.entry { border: var(--ssd-border-width) solid var(--ssd-border); border-radius: var(--ssd-radius-sm); padding: var(--ssd-space-2) var(--ssd-space-3); margin: 0 0 var(--ssd-space-2); background: var(--ssd-bg); min-width: 0; }
.entry[data-status="extra"], .entry[data-status="differs"] { border-color: var(--ssd-danger); background: var(--ssd-danger-bg); }
.entry[data-status="match"] { border-color: color-mix(in srgb, var(--ssd-ok) 55%, var(--ssd-border)); }
.entry-head { display: flex; justify-content: space-between; gap: var(--ssd-space-2); flex-wrap: wrap; font-size: var(--ssd-small-size); color: var(--ssd-muted); }
.entry-head b { color: var(--ssd-fg); }
.badge { font-size: 0.75rem; font-weight: var(--ssd-weight-strong); border-radius: var(--ssd-radius-pill); padding: 0 var(--ssd-space-2); white-space: nowrap; }
.badge.ok { color: var(--ssd-ok); background: var(--ssd-ok-bg); }
.badge.bad { color: var(--ssd-danger); background: var(--ssd-danger-bg); }
ol.tree, ol.tree ol { list-style: none; margin: 0; padding: 0; }
ol.tree ol { margin-left: var(--ssd-space-2); padding-left: var(--ssd-space-3); border-left: 2px solid var(--ssd-border); }
li.node { margin: var(--ssd-space-2) 0 0; min-width: 0; }
.sentence { font-weight: var(--ssd-weight-strong); overflow-wrap: anywhere; }
details { margin-top: var(--ssd-space-1); }
summary { cursor: pointer; color: var(--ssd-muted); font-size: var(--ssd-small-size); overflow-wrap: anywhere; }
summary:focus-visible, button:focus-visible { outline: 2px solid var(--ssd-accent); outline-offset: 2px; }
dl.args { display: grid; grid-template-columns: minmax(4rem, max-content) minmax(0, 1fr); gap: var(--ssd-space-1) var(--ssd-space-3); margin: var(--ssd-space-2) 0 var(--ssd-space-1); font-size: var(--ssd-small-size); }
dt { color: var(--ssd-muted); }
dd { margin: 0; overflow-wrap: anywhere; min-width: 0; }
code, .mono { font-family: var(--ssd-mono); font-size: 0.82em; }
.addr { white-space: nowrap; }
.doc { border: var(--ssd-border-width) dashed var(--ssd-border); border-radius: var(--ssd-radius-sm); padding: var(--ssd-space-2) var(--ssd-space-3); margin-top: var(--ssd-space-1); background: var(--ssd-surface); }
.doc ul { margin: var(--ssd-space-1) 0 0; padding-left: var(--ssd-space-4); }
.notice { border-radius: var(--ssd-radius-sm); padding: var(--ssd-space-2) var(--ssd-space-3); margin: var(--ssd-space-2) 0; font-size: var(--ssd-small-size); background: var(--ssd-warn-bg); color: var(--ssd-fg); border: var(--ssd-border-width) solid color-mix(in srgb, var(--ssd-warn) 45%, transparent); }
.notice.bad { background: var(--ssd-danger-bg); border-color: color-mix(in srgb, var(--ssd-danger) 45%, transparent); }
.notice.ok { background: var(--ssd-ok-bg); border-color: color-mix(in srgb, var(--ssd-ok) 45%, transparent); }
.preview { border-top: var(--ssd-border-width) solid var(--ssd-border); margin-top: var(--ssd-space-3); padding-top: var(--ssd-space-1); }
.card { border: var(--ssd-border-width) solid var(--ssd-border); border-radius: var(--ssd-radius-sm); padding: var(--ssd-space-2) var(--ssd-space-3); margin: var(--ssd-space-2) 0; background: var(--ssd-surface); }
.verdict { display: flex; gap: var(--ssd-space-2); align-items: flex-start; }
.verdict .icon { font-weight: 700; font-size: 1.1rem; line-height: 1.2; }
.verdict[data-allowed="true"] .icon { color: var(--ssd-ok); }
.verdict[data-allowed="false"] .icon { color: var(--ssd-danger); }
.verdict[data-allowed="null"] .icon { color: var(--ssd-muted); }
.meter { height: 0.6rem; border-radius: var(--ssd-radius-pill); background: var(--ssd-border); overflow: hidden; display: flex; margin: var(--ssd-space-2) 0 var(--ssd-space-1); }
.meter span { display: block; height: 100%; width: 0; transition: width var(--ssd-duration) var(--ssd-easing); }
.meter .spent { background: var(--ssd-muted); }
.meter .this { background: var(--ssd-accent); }
.meter .over { background: var(--ssd-danger); }
.scroll { overflow-x: auto; }
table { width: 100%; border-collapse: collapse; font-size: var(--ssd-small-size); }
th, td { text-align: left; padding: var(--ssd-space-1) var(--ssd-space-2); border-bottom: var(--ssd-border-width) solid var(--ssd-border); vertical-align: top; }
th { color: var(--ssd-muted); font-weight: 500; }
th.num, td.num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
.neg { color: var(--ssd-danger); }
.pos { color: var(--ssd-ok); }
ul.events { margin: 0; padding-left: var(--ssd-space-4); font-size: var(--ssd-small-size); }
.muted { color: var(--ssd-muted); }
.small { font-size: var(--ssd-small-size); }
footer { display: flex; gap: var(--ssd-space-2); align-items: center; justify-content: space-between; flex-wrap: wrap; padding: var(--ssd-space-3) var(--ssd-space-4); border-top: var(--ssd-border-width) solid var(--ssd-border); background: var(--ssd-surface); }
.footer-slot { color: var(--ssd-muted); font-size: var(--ssd-small-size); min-width: 0; }
.actions { display: flex; gap: var(--ssd-space-2); margin-left: auto; }
button { font: inherit; font-weight: var(--ssd-weight-strong); border-radius: var(--ssd-radius-sm); padding: var(--ssd-space-2) var(--ssd-space-4); border: var(--ssd-border-width) solid var(--ssd-border); background: var(--ssd-bg); color: var(--ssd-fg); cursor: pointer; min-height: 2.5rem; transition: background-color var(--ssd-duration) var(--ssd-easing), border-color var(--ssd-duration) var(--ssd-easing); }
button.primary { background: var(--ssd-accent); color: var(--ssd-accent-fg); border-color: var(--ssd-accent); }
button.danger { background: var(--ssd-danger); color: var(--ssd-danger-fg); border-color: var(--ssd-danger); }
button[disabled] { opacity: 0.55; cursor: not-allowed; }
.spinner { display: inline-block; width: 0.9em; height: 0.9em; border: 2px solid currentColor; border-right-color: transparent; border-radius: 50%; animation: ssd-spin 0.8s linear infinite; vertical-align: -0.1em; margin-right: 0.35em; }
@keyframes ssd-spin { to { transform: rotate(360deg); } }
@media (prefers-reduced-motion: reduce) {
  dialog[open], .spinner { animation: none; }
  .meter span, button { transition: none; }
}
`;
