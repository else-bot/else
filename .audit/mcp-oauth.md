# MCP OAuth checkpoint

Goal: an HTTP MCP's 401 creates a native owner authorization card; browser consent completes with PKCE and server-side encrypted token storage, then the original read resumes. Shared installation must not grant another bot account access.

Risk: secret handling, per-bot identity, persisted token lifecycle, actor-owned transport state, owner-request parking and continuation.

Models: Sol xhigh implementation and test workers. User prohibits Claude agents, so Opus risk routing and cross-family review are excluded. Independent permitted Sol review will check the combined change.

Design: reuse package actors for OAuth and per-bot transports; reuse durable owner questions for connection cards and scheduling. Desktop loopback callback forwards the code to the authenticated API. Alternative considered: public API callbacks require a browser-reachable server and expand unauthenticated routes; existing desktop email loopback pattern supports remote API installations without this requirement.

Units: backend discovery and encrypted credentials; owner HTTP/tools and durable card; desktop browser consent; fake Python provider E2E and independent review. No provider account is required. OAuth metadata/token endpoints use bounded nonredirecting requests. Read requests can retry after consent; mutations never replay automatically.

Verification: focused API integration and backend tests, desktop callback tests, workspace type checks, real desktop card in isolated omabox. Native macOS execution is unavailable on this Linux host.

Implementation decisions: the native card persists the exact issuer and scope list in safe approval metadata. Authorization returns the actual grant metadata and the desktop compares it before opening the browser. The callback must still belong to a pending card. Cancel is available while the browser is open and disabled once code exchange starts. Scope strings are bounded to fit the durable card. Reserved authorization query parameters are rejected. Stdio retains one shared process.

Two independent Sol reviews identified stdio sharing, consent drift, cancellation timing, card size, tenant OIDC discovery, and authorization query injection issues. All were fixed. The coordinator also caught safe metadata being discarded by approval serialization and corrected the MCP continuation instructions. The fake-provider test now checks both the persisted card metadata and the actual resumed provider packet.

Verification also exposed two baseline issues. Fresh databases failed because two migrations used version0019; the newer default-skills migration is now0020. The avatar-change test could select the bot's random initial avatar and incorrectly expect a change event; its fixture now chooses a different shape. Parallel backup tests were killed in isolation, so the final suite uses one test thread. Debug age translation resources are mounted read-only into the test box.

Native desktop evidence uses a seeded connection card in a disposable API session. It verifies rendering and cancellation, not a live provider login. Browser callback and token exchange use real HTTP in the separate desktop and fake-provider tests. No Sentry account was used. Native macOS remains unverified.

Final result: the complete workspace suite, formatting, Clippy and diff checks pass. Exact commands and scope limits are in `mcp-oauth-verification.txt`. The final risk check confirms bot-specific credentials, actor ownership, bounded requests, one-shot callbacks, and no automatic mutation replay. No required implementation work remains.
