# Email connectors design checkpoint

Bots need multiple inboxes, actual mail-event wakeups, normal mail actions, and private native connection forms. The reusable connector core must also support later messaging services.

This checkpoint contains research, a proposed architecture, three HTML alternatives, and an isolated dependency compatibility probe. The native app-settings shell is bootstrapped. The connector API and native email setup flow are not implemented yet.

- [Try the variants](variants.md) and open [A](variant-a.html), [B](variant-b.html), or [C](variant-c.html).
- [Read the provider and library research](research.md).
- [Read the proposed architecture and release gates](architecture.md).
- [Read the candidate synthesis](synthesis.md).
- [Inspect the checkpoint](checkpoint.md) and [decision trail](decisions.tsv).
- [Inspect prototype verification](prototype-verification.json), [rendered T3 attachment metadata](rendered-previews.json), and [dependency-check output](dependency-check.log).
- [Reproduce the dependency probe](../../experiments/email-connectors/README.md).

The JSON candidate and risk reviews are raw independent opinions. They contain rejected recommendations. The architecture and synthesis documents record the selected design and supersede those reports. In particular, Gmail Pub/Sub supports outbound pull, and provider labels cannot supply reliable cross-installation fencing.

Live OAuth, real inbox behavior, asynchronous driver cancellation, native GPUI behavior, and connector backup/restore remain unverified. The HTML simulates all account and mail operations.
