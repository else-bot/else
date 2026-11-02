# Product requirements

These requirements describe the planned assistant. See the [product overview](../README.md) for the motivation and the [architecture notes](architecture.md) for the application structure.

## First version

Start with one owner's messaging app. The owner can list bots, create a bot with a name and role, open its conversation, send messages, and delete the bot. Keep conversation history and memory persistent. The main assistant remains the default place to ask for help.

Build the supporting provider, memory, authentication, backup, and execution contracts with this scope in mind. Research fixtures verify individual design choices. They are separate from application implementation.

Email, kanban, notes, routines, native mobile apps, and additional desktop views remain product goals for later versions. The requirements below describe that broader direction.

## One assistant that understands my context

The main assistant should act as a copy of myself in the sense that it learns my circumstances, habits, and ways of working. It should recognize which part of my life a request belongs to and delegate to the appropriate bot when needed.

Context recognition and delegation should be entirely prompt-driven. The AI CLI connects the pieces, and the main assistant uses its prompt and context to decide which bot should handle a request.

The SQL database should store the available bots and their roles. The main assistant needs that information in its context so it knows which bots exist and what it can delegate to each one.

For example:

> Remind me to pick up the kids on Friday because my wife works late that day.

The assistant should create the reminder in the personal or family compartment of its memory without making me choose a bot or explain the context each time.

The same assistant should understand my work. It should learn from my daily tasks, my job, and my companies. Over time, it should become proactive enough to suggest what to do next and what we should tackle.

Each bot keeps its own memory. Skills available through the prompt let bots read other bots' memories when they need context about personal life, family, or work.

## Conversation and built-in tools

Messaging is the main entry point. I should be able to talk to the assistant, ask it to do something, and receive updates and notifications.

The interface should also let me open the things the assistant works with.

### Projects and kanban

A simple project app should support multiple projects and these statuses:

1. `BACKLOG`
2. `TODO`
3. `IN_PROGRESS`
4. `TO_REVIEW`
5. `DONE`
6. `ARCHIVED`

Bots should be able to use it directly, so I do not need to host a separate project app for them.

### Notes

I should be able to ask bots to take notes and find those notes later inside else.bot. Obsidian is a reference for keeping knowledge accessible, but the initial app should be much simpler. Something closer to Apple Notes on iOS and macOS would be enough.

### Email

An email client should be built in. Email is where much of the switching between family and work happens, with different notifications and contexts to handle.

The assistant should help organize email, create reminders from it, and draft replies.

## A computer for each bot

One thing I like about Grok Bot is that each bot has its own micro-VM. I can connect through VNC and see its personal computer.

else.bot should give each bot an isolated computer so its environment does not leak into another bot's environment. The initial setup should include Chromium, a desktop, and a terminal. We can add other tools as needs become clearer.

Bot memory is described in [memory and storage](memory.md). Scheduled and event-driven work is described in [routines and webhooks](routines.md). The bot computer lifecycle is described in [sandbox research](sandboxes.md).
