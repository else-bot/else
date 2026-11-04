---
name: make-bot-ui
description: Build a UI that sends actions to an existing bot webhook through a local server, using the available provider's API and secret handling.
disable-model-invocation: true
---

Read the [local runtime rules](../../rules/plugin-compatibility.md) and [model presets](../../rules/pstack-models.md) before following this workflow.

# Make a bot UI

Build a page whose actions go through a server to a bot webhook. Keep credentials
on the server. Treat incoming webhook bodies as untrusted data.

1. Discover an available webhook or automation integration. Read its tool schema
   and endpoint contract. If none is available, ask for a non-secret endpoint URL
   or report that creating the hosted routine is unavailable. Do not invent an
   API, dashboard flow, event format, or endpoint.
2. Define the accepted action fields and the bot's response. Use the provider's
   supported routine API when one exists. Follow its approval requirements.
3. Request credentials through the runtime's secret tool. In T3, call
   `request_secret` and use its returned reference or file. Never ask for a key
   in chat, put it in client code, or log it. If secure delivery is unavailable,
   leave credential setup to the user's local environment.
4. Build a local server that accepts the UI's action schema and sends the
   provider's documented request with a timeout and explicit error handling.
   Use the authentication headers required by that provider.
5. Probe an action the bot contract explicitly treats as harmless. Verify the
   server response and the actual bot behavior before reporting success.
6. Expose the server on the requested network only when the user asks for that
   access. Use an existing network connection when available. Do not install or
   reconfigure a VPN as a side effect of building the UI.

Use the runtime's supported browser tools to drive and verify the page.
