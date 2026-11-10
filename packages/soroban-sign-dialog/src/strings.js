// Every user-facing string the dialog renders, in English. A wallet replaces
// any of them with `dialog.strings = { ... }` (merged over these defaults).
//
// Values are inserted as HTML. Parameters arrive already escaped (addresses
// and amounts as small pieces of markup), so an override only has to escape
// text of its own.

const and = (xs) => (xs.length < 2 ? xs.join('') : `${xs.slice(0, -1).join(', ')} and ${xs.at(-1)}`);

export const STRINGS = {
  // Header and footer.
  networks: { testnet: 'Testnet', mainnet: 'Mainnet', futurenet: 'Futurenet', local: 'Local network' },
  customNetwork: 'Custom network',
  requestFrom: (p) => `Request from <b>${p.dapp}</b>`,
  titleOne: 'Authorize this call',
  titleMany: (p) => `Authorize ${p.count} calls`,
  titleTransaction: 'Approve this transaction',
  titleUnreadable: 'This request could not be read',
  titleEmpty: 'No request',
  summary: (p) => `Fee up to ${p.fee} XLM, paid by ${p.payer}`,
  reject: 'Reject',
  approveButton: 'Approve',
  approveAnyway: 'Approve anyway',
  confirmApproveAnyway: 'Confirm: approve anyway',

  // The summary at the top: one plain line per authorized call, naming the
  // payments you make in its sub-calls and counting the rest, and the
  // spending cap when the preview found one.
  summarySend: (p) => `Send ${p.amount} to ${p.to}`,
  summaryLine: (p) => `${p.lead}${p.payments.length ? `, paying ${and(p.payments)}` : ''}${p.more ? `${p.payments.length ? ',' : ''} ${p.more}` : ''}`,
  summaryPayment: (p) => `${p.amount} to ${p.to}`,
  summaryMore: (p) => (p.count === 1 ? 'and 1 more call' : `and ${p.count} more calls`),
  summaryUnusedTag: 'not used',
  summaryBudget: (p) => (p.over
    ? `Over the cap: ${p.left} of ${p.limit} ${p.code} left this window`
    : `${p.left} of ${p.limit} ${p.code} left this window after this`),

  // The simulated outcome, as one status.
  statusSuccess: 'Will succeed',
  statusRefused: 'Will be refused',
  statusUnknown: 'Outcome unknown',
  statusChecking: 'Checking the outcome…',
  statusIdle: 'Outcome not checked yet',
  simulatedOk: 'The simulation against current ledger state completed.',
  notSimulated: 'This request was not simulated.',

  // Warnings above Approve.
  unusedWarning: (p) => `This request also asks you to sign an authorization the transaction does not use: ${p.what}. If you approve, the signature is published with the transaction and anyone can submit that authorization later, until it expires.`,
  differsWarning: 'The simulation needs a different authorization from the one you are asked to sign.',
  notSimulatedWarning: 'This request could not be simulated. Approve anyway?',
  missingWarning: 'The transaction needs an authorization this request does not include, so it is likely to fail.',

  // Everything else, behind one disclosure.
  details: 'Details',

  // The request.
  nothingToReview: 'Nothing to review.',
  unreadable: (p) => `${p.error}. Nothing has been signed. Reject the request.`,
  whatYouAuthorize: 'What you authorize',
  authorizedBy: (p) => `${p.index}. Authorized by <b>${p.who}</b>`,
  sourceAccount: 'the transaction source account',
  addressBound: 'address-bound',
  sourceAuthority: 'Runs with the source account’s authority',
  operation: (p) => `${p.type} operation`,
  you: 'you',
  nativeAsset: 'XLM (native asset)',
  argument: (p) => `arg ${p.index}`,
  argumentCount: (p) => (p.count === 1 ? '1 argument' : `${p.count} arguments`),
  contract: 'contract',
  none: 'none',
  yes: 'yes',
  no: 'no',
  bytes: (p) => `${p.length} bytes`,
  seconds: (p) => `${p.value} s`,

  // Sentences for well-known calls. Amounts and addresses are markup.
  send: (p) => `Send ${p.amount} from ${p.from} to ${p.to}`,
  approve: (p) => `Let ${p.spender} spend up to ${p.amount} of ${p.from}’s balance until ledger ${p.until}`,
  transferFrom: (p) => `${p.spender} sends ${p.amount} from ${p.from} to ${p.to}`,
  burn: (p) => `Burn ${p.amount} from ${p.from}`,
  mint: (p) => `Mint ${p.amount} to ${p.to}`,
  deployAsset: (p) => `Deploy the ${p.code} asset contract`,
  deploy: (p) => (p.code ? `Deploy a new contract from code ${p.code}` : 'Deploy a new contract'),
  upload: (p) => `Upload contract code (${p.length} bytes)`,
  installGrant: (p) => `Install grant #${p.grant} on your account`,
  replacePolicy: 'Replace your account’s policy document',
  call: (p) => `Call <code>${p.fn}</code> on ${p.contract}`,

  // A policy document carried in a call's bytes argument.
  policyDocument: (p) => `Policy document (${p.network ?? 'network not named'})`,
  rules: 'Rules',
  signers: 'Signers',
  anyFunction: 'any function',
  ruleLine: (p) => `<b>${p.name}</b>: ${p.functions} on ${p.scope}${p.by ? ` by ${p.by}` : ''}${p.cap ? `; at most <b>${p.cap}</b> per ${p.period} ledgers` : ''}`,
  signerKey: (p) => `key ${p.key}`,

  // The authorization check.
  badgeMatch: '✓ Matches simulation',
  badgeExtra: '⚠ Not used by this transaction',
  badgeDiffers: '⚠ Differs from simulation',
  extraNote: 'This entry is not needed to run the transaction. If you sign it, the signature is published with the transaction and anyone can submit it later, until it expires.',
  differsNote: 'The simulation recorded a different authorization for this signer. Compare the trees before signing.',
  authMatch: '✓ The simulation recorded the same authorization tree independently.',
  authMismatch: (p) => `⚠ The simulation’s own recording of the authorization tree does not match the request: ${p.extra} not used, ${p.differs} different, ${p.missing} missing.`,
  neededNotSupplied: 'Needed but not supplied:',

  // The preview.
  effects: 'Effects',
  previewButton: 'Check outcome',
  previewUnavailable: 'Preview unavailable.',
  treeStillExact: 'The authorization tree in Details is still exact.',
  previewFailed: (p) => `Preview failed: ${p.error}`,
  wouldAccept: (p) => `<b>Would be accepted</b> by ${p.account}${p.rule ? ` under rule <code>${p.rule}</code>` : ''}.`,
  wouldRefuse: (p) => `<b>Would be refused</b> by ${p.account}${p.rule ? ` under rule <code>${p.rule}</code>` : ''}.`,
  verdictUnknown: (p) => `<b>Verdict unknown</b> for ${p.account}.`,
  contractError: (p) => `Contract error ${p.code}`,
  spendingCap: 'Spending cap',
  capRule: (p) => ` · rule <code>${p.rule}</code>`,
  capMeter: (p) => `${p.spent} spent of ${p.limit}; this request ${p.amount}`,
  capLeft: (p) => `${p.remaining} of ${p.limit} left in the current window of ${p.period} ledgers.`,
  capAfter: (p) => `After this: <b>${p.after}</b> left.`,
  capOver: (p) => `<b>This request (${p.amount}) is over the remaining cap.</b>`,
  capRelease: (p) => `The oldest spend leaves the window at ledger ${p.ledger}${p.minutes != null ? ` (about ${p.minutes} min)` : ''}.`,
  balanceChanges: 'Balance changes',
  balanceHeaders: ['Account', 'Asset', 'Before', 'After', 'Change'],
  noBalanceChanges: 'No balance changes.',
  events: 'Events',
  transferEvent: (p) => `<b>transfer</b> ${p.amount} from ${p.from} to ${p.to}`,
  undecodableEvent: (p) => `undecodable event (${p.error})`,
  wouldFail: (p) => `The transaction would fail: ${p.error}${p.codes ? ` (contract error ${p.codes})` : ''}`,
  footnote: (p) => `Simulated locally${p.ledger ? ` at ledger ${p.ledger}` : ''}, protocol ${p.protocol}, with ledger state from ${p.source}. The result is a prediction, not a guarantee.`,

  // Notices the engine reports by code (PreviewResult.code). The engine's
  // own English `notice` is used for codes not listed here.
  notices: {
    protocol: (p) => `The network is on protocol ${p.protocol}, and this preview supports protocols ${p.builds.join(' and ')}. Effects are not shown, because a simulation under other rules can be wrong.`,
    'no-rpc': 'No RPC is configured for this network, so ledger state cannot be read.',
    'no-transaction': 'A preview needs the whole transaction. For bare authorization entries the dialog shows the tree only.',
    'fee-bump': 'Preview the inner transaction of a fee bump.',
    'not-invoke': (p) => `This ${p.operation} operation authorizes nothing; there is nothing to preview.`,
    'no-hint': 'No signer hint was given, so the account’s own policy was not evaluated; effects assume every authorization passes.',
  },

  // Account verdict reasons by code (verdict.reasonCode).
  reasons: {
    accepted: (p) => `Accepted${p.rule ? ` under ${p.grant != null ? `grant ${p.grant}, ` : ''}rule ‘${p.rule}’` : ''}, assuming a valid signature from this key.`,
    refused: (p) => `The account refuses it${p.cause ? ` because of ${p.cause}` : ''} (contract error ${p.code}).`,
    'refused-over-cap': (p) => `The account refuses it${p.cause ? ` because of ${p.cause}` : ''} (contract error ${p.code}). The request is more than the cap has left in this window.`,
    failed: (p) => `The enforcing simulation failed: ${p.error}.`,
    'unsigned-others': 'Other parties have not signed yet, so this account’s verdict is unknown.',
  },
};

/** Defaults with `overrides` merged in, one level deep for the nested maps. */
export function mergeStrings(overrides = {}) {
  const out = { ...STRINGS, ...overrides };
  for (const k of ['networks', 'notices', 'reasons']) out[k] = { ...STRINGS[k], ...(overrides[k] ?? {}) };
  return out;
}
