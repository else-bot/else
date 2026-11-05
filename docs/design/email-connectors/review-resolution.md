# Independent review resolution

Reviewed by claude-fable-5-1 at high effort. The raw report is in independent-review.json. The reviewer agrees with the account-owned base and confirms the evidence is labeled honestly.

## Findings resolved

1. **OAuth application ownership.** The proposed default is project-owned public clients for desktop-assisted PKCE setup. API-only installations can configure their own registered applications. Client configuration belongs to the installation; mailbox tokens belong to connections. Provider approval and redirect compatibility remain live-spike gates.
2. **Password-clearing check.** The script now holds the submitted input reference and verifies its value is cleared after authorization. It also verifies the credential is absent from serialized prototype state. All six desktop and narrow-width runs passed again. This proves prototype behavior only.
3. **IMAP account identity.** The architecture distinguishes a provider-authenticated OAuth subject from an accepted IMAP login principal. IMAP account identity uses the TLS-verified host and authenticated username. Display email remains owner-asserted; SMTP sender identities need separate verification.
4. **Owner-facing asks.** Connector-origin turns cannot use `ask_owner` or permission requests. Trusted application health and reconnection notices carry connection provenance.
5. **Graph expiry evidence.** The primary subscription documentation was checked through the web tool before review and lists 1,440 minutes for rich notifications. The reviewer did not independently refetch it. The report now states that limit and includes returned expiration and account support in the live spike.

The reviewer regenerated all three variants and found byte-identical output. The reviewer did not rerun the crate probe, preview the HTML, fetch provider documentation again, or audit a transcript file. The parent ran the compilation and preview checks recorded here.

No comments were deleted or restored. No comment-review rerun or constraint encoding was needed. Architecture synthesis is documented separately. No production code or AGENTS rules changed. No commit or PR was created; the probe lockfile exists in the working tree.

## Remaining attention

The dominant remaining risk is unverified live provider behavior and asynchronous driver cancellation. Source inspection and compilation do not establish working account integration. Native implementation awaits the user's requested design direction.
