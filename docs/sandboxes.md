# Bot computers and sandbox research

Micro-VMs are the preferred isolation direction. Keep Microsandbox for the first else.bot prototype, pinned to v0.7.13. Local checks on November 2, 2026 passed for separate bot filesystems, lifecycle, Chromium, VNC, snapshots, and the Rust SDK. The [verification report](experiments/bot-vm-verification.md) records the measurements and limits. The [product requirements](product.md#a-computer-for-each-bot) describe the intended bot computer.

## Sandbox research

Initial research, October 25, 2026. Local verification, November 2, 2026.

Micro-VMs remain the preferred isolation direction. The local runtime must create and manage bot computers without requiring a Docker account or another infrastructure vendor account. Authentication to the owner's AI subscriptions remains a separate requirement.

Docker Sandboxes does not meet this requirement because it requires Docker sign-in for local use. Its maintenance and isolation features are useful references, but it is excluded from the proposed deployment while that requirement remains. [Docker Sandboxes FAQ](https://docs.docker.com/ai/sandboxes/faq/).

OCI images are still a useful way to package each bot's environment. The runtime determines the isolation. Ordinary containers share the host kernel, a micro-VM has its own guest kernel, and gVisor provides a userspace kernel without being a hardware VM. [Container and VM isolation](https://docs.docker.com/ai/sandboxes/security/isolation/), [gVisor architecture](https://gvisor.dev/docs/architecture_guide/security/).

Microsandbox's documentation describes local operation without an account and a native Rust SDK. The local tests used a private home directory without existing credentials and confirmed both paths. Provider subscription authentication and the complete else.bot lifecycle manager still need integration tests. [Local operation](https://microsandbox.dev/), [SDK and runtime overview](https://docs.microsandbox.dev/getting-started/introduction).

## Existing projects to evaluate

| Project | What we can reuse | Tradeoffs for else.bot |
| --- | --- | --- |
| [Microsandbox](https://github.com/superradcompany/microsandbox) | Local micro-VMs, OCI images, a Rust SDK, and a CLI for named sandboxes, command execution, snapshots, and volumes. | Local use requires no account. The project is beta software and warns of breaking changes. Validate desktop compatibility and lifecycle behavior before adoption. |
| [OpenSandbox with Kata Containers](https://github.com/opensandbox-group/OpenSandbox/blob/main/docs/guides/secure-container.md) | A sandbox lifecycle API and existing desktop examples, with a VM-backed runtime. [Kata overview](https://github.com/kata-containers/kata-containers). | Adds a Python management service and container runtime integration. Configure the VM-backed runtime explicitly. Its ordinary container backend does not provide a guest kernel per bot. |
| [Firecracker](https://github.com/firecracker-microvm/firecracker) | An open-source micro-VM monitor written in Rust and using Linux KVM. | A lower-level option. We would need to provide or adopt guest images, networking, command execution, and persistent bot lifecycle management. |

OpenSandbox's desktop example and AIO Sandbox remain references for building the guest environment. They are separate from the choice of isolation runtime. AIO's quick-start disables Docker's seccomp filter and leaves services open unless an API key is set, so copying its defaults would need review. [OpenSandbox desktop](https://github.com/opensandbox-group/OpenSandbox/blob/main/docs/examples/desktop.md), [AIO Sandbox](https://github.com/agent-infra/sandbox).

## Microsandbox maintenance and deployment

The latest release checked and tested was v0.7.13, published on November 1, 2026. The Linux bundle matched its published SHA-256 digest. The project still labels itself beta software, so pin and test releases. [Release notes](https://github.com/superradcompany/microsandbox/releases/tag/v0.7.13), [project status](https://github.com/superradcompany/microsandbox).

Microsandbox is Apache 2.0 licensed. Its local Linux runtime requires KVM. Its documentation describes running micro-VMs directly from the SDK without a daemon or remote service. A hosted server must expose the required virtualization support. [Project requirements](https://github.com/superradcompany/microsandbox), [local runtime model](https://docs.microsandbox.dev/getting-started/introduction).

## Microsandbox footprint and Rust deployment

The Rust SDK has an async API for creating micro-VMs, executing commands, transferring files, and configuring networking. Deployment includes a separate runtime executable and firmware alongside the application. Each bot's micro-VM runs in its own host process. [Rust SDK](https://github.com/superradcompany/microsandbox/blob/main/sdk/rust/README.md), [runtime setup](https://docs.microsandbox.dev/sdk/setup).

The published numbers describe different parts of startup:

| Measure | What the upstream sources establish | Meaning for else.bot |
| --- | --- | --- |
| Guest boot | The project claims an average below 100 ms for guest boot on an M1 machine. [README and benchmark footnote](https://github.com/superradcompany/microsandbox). | This does not measure downloading an image or starting Chromium and the desktop. |
| Small Linux workload | The upstream benchmark reports a 320 ms median from CLI invocation through process exit for an Alpine workload on a bare-metal Linux host. The results are dated May 12, 2026. [Benchmark methodology and results](https://github.com/superradcompany/sandbox-bench/blob/main/benches/boot-time/README.md). | Useful evidence of short startup times for a small guest. It is an upstream measurement on different hardware and software, not an else.bot desktop benchmark. |
| Guest memory | The Rust SDK defaults to 512 MiB. It allocates physical pages as the guest touches them. [Memory configuration](https://docs.microsandbox.dev/sdk/rust/sandbox). | This is the guest memory setting. It does not state the runtime's host memory overhead or what our browser workload needs. |

The local test used 1 vCPU and 2 GiB guest memory. A restored desktop with one local Chromium page used about 563 MiB of host VM-worker RSS. The first desktop run retained about 1.49 GiB after package installation, including guest caches. These are samples of a small workload. Keep the 2 GiB budget for the prototype and measure actual websites and guest tasks before treating it as a requirement. Budget host provider processes separately. See the [resource measurements](experiments/bot-vm-verification.md#resource-behavior).

The simplest proposed deployment is a native Linux service for the Rust API with access to `/dev/kvm`. Provision a writable runtime home and image cache, a pinned `msb` executable, and its matching `libkrunfw` library, which contains the guest kernel. Preload the guest image so the first conversation does not wait for a download. The SDK handles command execution through its guest-agent channel without requiring SSH. [Linux setup and startup sequence](https://docs.microsandbox.dev/getting-started/quickstart), [runtime provisioning](https://docs.microsandbox.dev/sdk/setup).

For this deployment, use the Rust crate's `local` and `net` features with Cargo defaults disabled. The SDK also offers build-time runtime downloads and an optional embedded runtime archive. Choose a controlled installation step for the matching runtime files. Embedding can simplify distribution, but the runtime still needs an executable location on disk. [Cargo features](https://github.com/superradcompany/microsandbox/blob/main/sdk/rust/README.md), [offline and embedded setup](https://docs.microsandbox.dev/sdk/setup).

The proposed deployment needs an API service, runtime assets, and local VM storage. The local tests used no separate sandbox management server. A repeatable desktop image, subscription authentication, and application recovery still need integration work.

## Controlling micro-VMs from Rust

The Rust SDK test created a named VM, executed a command, stopped it, and reconnected with `connect_or_create()`. Its saved file and original resource settings survived. The temporary crate used v0.7.13 with Cargo defaults disabled and the `local` and `net` features. The [SDK fixture](experiments/bot-vm-verification.md#rust-sdk-check) records the exact test. else.bot still needs an adapter for finding, starting, stopping, and accessing each bot computer.

The SDK documents `connect_or_create()` for reusing a named environment. Existing configuration wins, and builder settings apply only when creating a new one. It also supports detached sandboxes that survive the creating process. `connect_or_create()` starts stopped environments, so else.bot must check the desired running state in PostgreSQL before calling it. Avoid the destructive `.replace()` option during normal startup. Validate detached operation and reconnection across API restarts with the pinned release. [SDK lifecycle reference](https://docs.microsandbox.dev/sdk/rust/sandbox).

The target is one persistent micro-VM environment per bot, including the main assistant. Its identity and saved filesystem persist while its compute can pause or stop between uses. A provider switch should reuse that bot's computer. Provider CLIs run on the host and operate the VM through scoped else.bot tools. Conversation handoffs and native provider sessions remain on the host under else.bot's control.

## First-version execution lifecycle

Use one scheduler in the Rust API and durable PostgreSQL records. Track host provider turns, guest jobs, and VM leases independently. Allow one active provider turn and one mutating guest operation per bot initially. Other bots can run within the configured host budget.

Persist intent before external dispatch and confirm the actual outcome before releasing reservations. A running guest job retains its lease after a provider switch or client disconnect. Expiry requests reconciliation. It does not prove that work ended. Unknown dispatch outcomes stay unknown until inspected and must not trigger automatic retries.

Keep bot status separate from desired computer state. An explicitly stopped computer permits host conversation and memory work, but guest tools cannot wake it until explicit resume. Soft deletion revokes access and stops all work while retaining chats, memory, native session files, and VM disks for later explicit purge. Startup preserves tombstones and never replaces a missing recorded computer with an empty environment.

The [execution-lifecycle research](research/execution-lifecycle.md) defines the records, scheduler pseudocode, resource admission, deadlines, and restart checks. Its fixture passes 28 interleaving and crash cases plus nine edge checks. Real runtime job recovery, SQL races, process containment, and dispatch correlation remain unverified.

## VM locks and resource management

Bots must acquire a VM lock through the Rust API before using their computer and release it when the work finishes, fails, or is cancelled. The lifecycle manager starts or resumes the VM when needed. The proposed 1 vCPU and 2 GiB budget applies to an active desktop bot.

Running tasks, background jobs, and open VNC sessions all count as active use. The manager tracks each holder and only considers the VM idle after all locks are released. It must serialize conflicting use of the same desktop and prevent an idle shutdown from racing with a new lock acquisition.

Locks should have renewable leases so a crashed caller cannot keep a VM running forever. Reconcile expired leases with actual running work before reclaiming the VM. Acquisition and release must be safe to retry, and API restarts must reconcile saved lock state with live VM activity. Code must enforce this lifecycle, including cleanup on errors.

After the final release and a configurable idle timeout, stop the VM when RAM must be reclaimed. The local pause test retained about 190 MiB of worker RSS, while stop removed the worker process. Pause can still serve brief idle periods within the memory budget. Count paused VMs' retained memory until it is released. Both operations must preserve the bot's saved filesystem.

Conversation, memory, and connector work can continue without acquiring a VM lock. Acquire locks only when a tool needs the bot computer, and retain them for live guest jobs or desktop use. Host CLI activity alone must not keep an idle VM running.

Configuration should set the maximum active VMs, total CPU and memory budgets, per-bot limits, and idle timeout. Reserve resources for the host provider processes, Rust API, and PostgreSQL. Queue new VM usage when the available budget is exhausted. Dedicated bot environments should not require every bot's VM to run at once.

## Desktop and host tool integration

The proposed guest image should include Chromium, a desktop, a terminal, a VNC server, noVNC, and task utilities. Provider CLIs and their subscription credentials stay on the host. We can reuse the desktop setup from the existing projects as a reference. Validate the complete image with the selected micro-VM runtime.

The host agent uses the [computer skill and tools](providers.md#host-to-vm-tools-and-skills) to execute guest work and inspect results. The Rust API manages the guest-agent connection and VM lifecycle. Guest software receives only the task-specific credentials it needs. It receives no general access to the host or other bots.

Validate provider login, token renewal, and saved CLI sessions separately on the host. An account-free VM runtime does not remove the need to authenticate with the chosen AI provider.

## Lifecycle and recovery

PostgreSQL should record each bot's identity, pinned image version, runtime configuration, and desired running state. Startup should recreate missing environments and preserve existing ones. It must respect bots that the owner has stopped.

The chosen runtime must preserve each bot's saved filesystem across normal stops and restarts. Normal else.bot startup must never remove, prune, or reset existing environments. Changes that require replacing an environment belong in an explicit upgrade migration after preserving its data.

Client disconnection must not interrupt work that still holds a VM lock. Once no active use remains, the idle policy can pause or stop the computer. Validate how the runtime handles process ownership, detached operation, and API restarts. Startup must not wake an owner-stopped bot just to configure its desktop connection.

Back up each bot's saved filesystem and browser profile through encrypted VM snapshots alongside the PostgreSQL export. PostgreSQL keeps application records and references to those artifacts. VM files do not need a second copy inside the database. A new server should restore the matching database and VM backup set without overwriting existing environments during normal startup.

VM archives preserve guest files and browser state. They do not contain the host's provider authentication or native CLI sessions. The [backup inventory](backups.md#host-provider-state) must cover required host provider state separately through supported interfaces, or require a fresh login and rebuild native context from PostgreSQL. Test subscription token renewal and any required provider login before claiming that recovery restores provider access.

## Validation before adoption

The first implementation investigation should establish these facts:

- A clean headless server can create and reuse two separately named bot micro-VMs without an infrastructure vendor login.
- Chromium, the desktop, and VNC streaming work in the custom environment. Host CLIs can operate them through scoped tools without exposing host execution access.
- Switching providers preserves the bot's computer and continues its task with else.bot's handoff context.
- Disconnecting clients preserves work with active VM locks. Releasing the final lock allows the idle policy to pause or stop the VM. Owner-stopped bots remain stopped after else.bot restarts.
- Failed tasks and crashed callers release or expire their locks without interrupting other active users. Concurrent acquisitions and idle shutdowns cannot race.
- New VM usage queues when the server resource budget is exhausted, and idle VMs reclaim the resources expected by the configured policy.
- Files, browser profiles, and direct network access remain separate between bots. Required API access and resource limits work.
- Idle and active memory use, startup time, and VNC responsiveness fit the target server's resources.
- A matching database export and VM snapshot set restore the bot definitions, files, conversations, and provider access on a new server. Repeating startup preserves that state.
- Snapshot upload retries and retention preserve every dependency of a completed backup. Restoring a change-only archive with a missing base fails without replacing an existing VM.

These remain acceptance criteria for the full application. The November 2 runtime tests covered separate filesystems, stop and pause behavior, desktop access, snapshot restoration, and native Rust control. Provider logins, API-managed leases, global resource scheduling, encrypted remote backups, and hostile-workload isolation still need validation.

[Isolation and remote access](security.md#isolation-and-remote-access) records the security requirements. [Snapshot findings and backup design](backups.md) cover saved state. The [snapshot experiments](experiments/vm-snapshots.md) retain the initial KVM blocker alongside the new measurements.
