# Application architecture

These are the current application choices. else.bot is still in research and design. No application implementation has started.

The project should use a monorepo with an API, a desktop client, and a mobile app.

## API

The API is the main entry point for clients and external tools. It should run on a server or dedicated hardware at home and expose an API and MCP access so other tools can connect.

The API should be written in Rust.

## Desktop client

The desktop client should use Rust and GPUI and connect to the API.

The main view should feel like a messaging app, similar to Grok Bot. Tabs across the top, like a browser, should let me open email, kanban, notes, and conversations.

## Mobile app

The current preference is to build native mobile apps, starting with iOS. The technology decision and mobile research are deferred until later.

Push notifications for new bot messages remain a requirement.

## Service responsibilities

The Rust API owns application state and client access. PostgreSQL stores that state, while bot computers retain their files in isolated environments. The clients connect to the API.

The detailed responsibilities are recorded in [provider integration](providers.md), [memory and storage](memory.md), [sandbox lifecycle](sandboxes.md), and [backup design](backups.md). [Startup requirements](restore.md#startup-must-preserve-existing-state) cover preservation of existing data and settings.
