# Telegram and Discord

A bot can read and reply in Telegram chats and Discord channels through a platform bot you create. You approve every message draft before it is sent.

## Connect

Ask your assistant and say what you want it to do. For example:

> Connect Telegram so you can summarize messages that mention you and draft replies.

The assistant opens a private connection card in the conversation.

1. Create a platform bot.
   - Telegram: message [@BotFather](https://t.me/BotFather) and send `/newbot`.
   - Discord: create an application in the [Developer Portal](https://discord.com/developers/applications), copy its bot token, and install the bot in your server.
2. Paste the token into the card's private field. The server stores it encrypted, and the assistant never sees it. Never paste tokens into a chat message.
3. Pick the conversations the bot may use and write its instructions.

Instructions describe how the bot should behave. They cannot grant access beyond the conversations and permissions you picked.

## Telegram notes

- The bot sees messages in chats it has joined, subject to Telegram's privacy settings. It cannot read your personal inbox or older history.
- To add a group, add the bot to it and send a message there.
- A public channel or group also works by its `@username` or `t.me` link, as in "post Test in @my_channel".
- To post in a channel, make the bot an administrator.

## Discord notes

- The bot sees only the channels its server permissions allow.
- To read normal message content, turn on the Message Content Intent in the Developer Portal.

## Manage connections

Open **Settings → Socials** to edit instructions and access, replace a token, pause a connection or disconnect it.
