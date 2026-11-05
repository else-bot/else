# Connector architecture synthesis

The two independent candidates were account-owned built-in adapters and a unified engine with a private transport seam. Both rejected mandatory sidecar state ownership. Both put credentials outside guest task secrets and queued actual mail events through the existing scheduler.

## Rubric and owner assessment

Scores use a five-point scale and are design judgments, not measured performance.

| Criterion | Account-owned core | Unified engine |
| --- | --- | --- |
| Credential isolation and grant enforcement | 5 | 4 |
| Event durability, restore, and external effects | 5 | 3 |
| Extension boundary beyond email | 5 | 3 |
| Packaging and deployment cost | 5 | 5 |
| Fit with current source | 4 | 4 |

Choose the account-owned core. A per-account actor prevents one busy inbox from serializing unrelated connections. A typed email module avoids making future messaging connectors implement email semantics. One registry can still derive UI and metadata without each provider owning separate policy tables.

Graft the unified candidate's trusted preset table and pure wake-rule evaluation. Keep generic connection lifecycle and delivery policy centralized. Reject its suggestion to restart restored accounts automatically, its arrival uniqueness without UIDVALIDITY, and its permissive generic polling default. A local database cannot prevent a restored clone from replaying old sends. Keep restore passive and classify uncertainty explicitly.

The parent adds provider transport choices, nonblocking actor I/O, separate control admission, payload binding for encrypted credentials, and explicit connector-origin authority. These are proposals requiring implementation tests.

Cross-family risk review uses claude-opus-5-5 at medium effort. A separate claude-fable-5-1 high review checks synthesis, prototype claims, comments, and audit evidence. Their conclusions are recorded in the checkpoint when available.

No source implementation was delegated. This checkpoint precedes the user-requested design choice.

The independent Fable reviewer agreed with the base. Its score adjustments favored the account-owned design further, primarily for source fit and extension beyond email. Five bounded findings were resolved directly and the affected prototype checks were rerun. The final attention items are live provider behavior, async-driver proof, registration, native implementation, and recovery verification.
