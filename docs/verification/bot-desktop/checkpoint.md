# Bot desktop checkpoint

The owner can wake a sleeping bot computer from its right sidebar, see its screen, expand it inside the app, watch without sending input, and take over the same Chromium session used by CDP.

- Blocking first steps: trace the existing guest services and computer leases; check native rendering and the streaming protocol before UI integration.
- Independent workstreams: API transport and ownership are separate from native presentation. Each owner edits its assigned files only.
- Shared mutable state: one streaming task owns each connection, with bounded messages. Computer admission remains scoped to each bot.
- Smallest safe decomposition: one backend risk owner, one native UI owner, followed by independent review and live verification.

risk: desktop session ownership, authentication, bounded streaming, and cancellation. Opus medium owns the backend decision and implementation. Sol xhigh fast owns native presentation and integration. Fable high reviews Sol scope; a fresh Sol reviews Opus scope.

Acceptance checks include asleep without implicit wake, explicit wake, live preview, expansion, no input while watching, pointer/key/scroll input under take-over, release back to watching, bot switch and window close cleanup, owner stop/deletion disconnect, and CDP access to a browser session changed by manual input. macOS arm64 execution is a handoff gate.

Actual presets were Sol xhigh fast for coordination, native presentation, client transport, and independent backend review; Opus medium for backend ownership and the guest lock; Fable high for native correctness, comments, and the decision trail. No provider substitution occurred.

Three review passes examined the implementation. The first addressed native releases, motion backpressure, scroll handling, connection bounds, resize ordering, and ownership. Live testing then caught Tab interception, coordinate-canvas placement, cold-start readiness, and stop/disconnect error ordering. The final Fable review also caught non-fatal takeover notices and initial-boot receive timing. Both were fixed. Leaving Details open no longer holds a computer lease. Source tracing and final native checks also confirmed that closing the sidebar releases its viewer lease.

The final amd64 verification and macOS boundary are recorded in [the handoff](handoff.md). The decision trail is [decisions.tsv](decisions.tsv). Chat-transcript audit is unavailable because no workspace-scoped transcript path was supplied. File, test, and live-artifact evidence is retained.
