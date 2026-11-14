# Getting started

## First run

For a server on this machine, choose **Start local**. The app reads the installation's owner key and starts the server if it is not running.

The welcome screen asks for your assistant's name, provider and model. Your assistant is the first bot in the sidebar.

To use a server on another machine, see [Remote access](remote-access.md).

## Chat

Select a bot and type a message. Enter sends and Shift+Enter adds a line. Closing the app leaves the server and any running work going.

Each bot keeps its own conversation, memory and rules. Open bot settings to change its provider or model. Open its **Computer** tab to watch its desktop.

## Create more bots

Press Ctrl/Command+Shift+N, or ask your assistant to make one. For example, "Create a bot that reviews my pull requests every morning." A bot can hand a task to another bot and get the answer back in its own conversation.

## Answer requests

Bots stop and ask when they need you. The request shows up as a card in the conversation and in the **Needs you** list:

- A question with choices.
- Permission for an action, such as publishing something.
- An email or message draft to approve before it is sent.
- A password or API key, typed into a private field instead of the chat.
- Help on its computer, such as solving a captcha or signing in.

## Run work on a schedule

Ask a bot to do something later or on a repeat. Bots can set routines ("every weekday at 9"), one-off reminders, and webhook triggers that start a turn when another service calls a URL. The bot's Details panel lists them.

## Extend bots

- [Email](email.md) connects Gmail or Microsoft 365.
- [Telegram and Discord](telegram-discord.md) connects chat bots.
- Ask a bot to install an MCP server. Every bot can then use it, and you sign in once from a card.
- Ask a bot to install a skill from a GitHub URL. Settings lists installed MCP servers and skills.

## Shortcuts

Use Ctrl on Linux and Command on macOS.

| Shortcut | Action |
| --- | --- |
| Ctrl/Command+Shift+N | Create a bot |
| Ctrl/Command+Shift+X | Stop the selected bot's work |
| Ctrl/Command+Shift+B | Open bot settings |
| Ctrl/Command+Shift+G | Create a sidebar category |
| Ctrl/Command+Shift+M | Move a bot to a category |
| Ctrl/Command+, | Open app settings |
