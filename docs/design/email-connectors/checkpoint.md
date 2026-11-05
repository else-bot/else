# Email connectors checkpoint

Status is research and interactive HTML prototypes before production implementation, as requested.

The user wants bots to work across multiple inboxes, react to actual incoming mail without paid empty prompts, perform normal mail actions, and connect accounts through private native forms. Contributors need a reusable connector contract for later messaging services.

## Workflow

1. Ground current API, SQLite persistence, scheduler, grants, private secret forms, and backup capture.
2. Research primary provider documentation and maintained Rust mail libraries. Separate verified library capabilities from integration assumptions.
3. Compare built-in Rust adapters with a mail sidecar. Define state ownership, credentials, event delivery, actions, recovery, and extension boundaries.
4. Build three interactive HTML connection flows with Gmail and iCloud paths, account grants, wake rules, and connection health.
5. Preview and render the variants in T3 Code. Review design and security assumptions independently.
6. Hand back the variants and a staged implementation contract. Native UI implementation waits for the user's design direction.

Done for this checkpoint means primary-source research is cited, current source ownership is recorded, three navigable prototypes are saved and shown in T3, and implementation gaps are explicit. This does not mean live inbox integration is complete.

Rigor is high for credential handling, event durability, and external mail effects. HTML work is reversible and uses fictional accounts only.

risk: proposed permission checks, credential storage, actor ownership, bounded queues, persisted connector lifecycle, and external-effect retry guarantees. Production invariants are unchanged in this checkpoint.

Actual coordinator is gpt-6.1-sol, xhigh, priority. Architecture source exploration uses the same native preset. Cross-family candidate and risk decisions use the repository presets. No application code slice is assigned because this checkpoint precedes implementation.

Throughput checkpoint: research, source tracing, and HTML authoring proceed independently. No paid model invocation is proposed for idle connection maintenance. The next dependency is confirming crate capabilities before settling the adapter boundary.

## Current evidence

- The isolated five-crate dependency probe passed under Rust 1.98. See dependency-check.log. This proves compilation only.
- Six T3 HTML preview runs passed interaction checks across six providers and widths 728 and 390. Desktop and narrow screenshots were inspected directly, including the permissions stage. See prototype-verification.json. These exercise mocked UI behavior only.
- The native Sol architecture candidate and delegated Fable candidate were read and compared. Synthesis selects account-owned adapters and grafts a single trusted registry plus pure wake-rule evaluation.
- Opus medium decision review confirmed host-only credential ownership, atomic cursor/event/delivery persistence, connector-origin authorization, deny-by-default MCP dispatch, revocation fencing, and passive restore. Its decision-only assignment has no code slice because implementation awaits the requested preview.
- Two risk-review suggestions were rejected after checking primary sources. Gmail Pub/Sub can use outbound pull without a public endpoint. A provider-side marker is not a reliable cross-installation fencing authority. The design states both limits explicitly. Polling remains a disclosed fallback, never a hidden model routine.
- Rework consists of one direct documentation pass after review. No production code, dependencies, migrations, or AGENTS rules were changed.

## Open work

Native flow selection, application implementation, OAuth registrations, provider delivery infrastructure, live account tests, cancellable protocol-driver proof, and encrypted connector recovery drills remain open. No full email feature is claimed.

The audit is checked against this conversation and inspected artifacts. No workspace-scoped transcript resource was supplied, so a separate transcript-file audit is unavailable.

All three variants were rendered inline in T3 and the optional design-direction question is pending. The recorded attachment IDs are in rendered-previews.json.

## Independent review completion

Fable high agreed with the selected base and identified five documentation/check gaps. The parent resolved OAuth application ownership, prototype input/state clearing assertions, IMAP identity wording, connector-origin owner-ask denial, and Graph expiry evidence labeling. All six interaction runs passed again after the assertion change. See review-resolution.md and independent-review.json.

The final risk remains proposed future production invariants; this diff only adds docs, HTML, verification artifacts, and an isolated protocol dependency probe. Rework rounds are one risk-review documentation pass and one independent-review correction pass. No application release or live provider claim is made.
