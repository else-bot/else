# PR132 maintainability review

Scope: PR132 against871a480. User requests accepted findings fixed in separate commits. No Claude agents.

Risk: actor resource identity, async secret persistence, native consent lifecycle and callback validation. Sol xhigh implementation and independent review follow the user's provider restriction.

Accepted findings:

1. Consent is encoded as fake tool arguments and persisted through loggable_args. Replace it with a typed connection action; give the native card its own implementation.
2. Stdio disconnect chooses a different session key from request/status. Bind storage to the session and normalize resource identity once.
3. New OAuth code combines protocol/state/storage/tests across giant modules. Separate cohesive owners and move encryption/filesystem durability off Tokio workers.
4. Token storage succeeds before card settlement. Make connected-but-unsettled state explicit, recover it on reopening, and retry only card confirmation.
5. Desktop email and MCP duplicate security-sensitive callback parsing and listener lifetimes. Use one typed loopback callback implementation with caller-specific provider policy.

Independent reviewer: gpt-6.1-sol. No comment deletions or suppression fixes were required. Cross-family review is excluded by the user's Claude restriction.

Resolution: fixes are separate commits for shared callbacks, session-bound async storage and decomposition, typed consent and isolated native cards, preserving existing accounts on reauthorization failure, and recovery of connected-but-unconfirmed cards. The re-review accepted the resulting ownership boundaries. Its final removed-package404 finding is fixed and tested; known absence permits cancellation while authentication/transport failures remain uncertain.

Verification: see `mcp-oauth-review-verification.txt`. The complete workspace suite, focused regression checks, formatting and Clippy pass. The native app automatically confirmed a pending card after recovering its saved credentials from the fake provider. The screenshot and repeatable driver are adjacent. No comment deletions or suppression changes were needed. The final diff retains actor ownership, bounded communication, per-bot secrets and no automatic mutation replay.
