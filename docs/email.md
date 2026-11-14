# Email

Bots can read, file and draft mail in a Gmail or Microsoft 365 mailbox. Every outgoing email shows up as a draft card, and nothing is sent until you approve it.

## Connect a mailbox

1. Open app settings and select **Email**.
2. Choose Gmail or Microsoft 365. For Microsoft, enter the mailbox address first.
3. Click **Connect Gmail** or **Connect Microsoft 365**.
4. Sign in with your provider in the browser and allow mailbox access.

Keep the app open until it confirms the connection. This also works when the app is connected to a remote server.

## Where tokens live

The server stores account tokens encrypted and refreshes them directly with Google or Microsoft. No else.bot sign-in server is involved.

Official releases ship with a Google Desktop client and a Microsoft application ID. Anyone can extract that configuration from the app, but mailbox tokens stay on your server.

## Limits

- Mail goes through IMAP and SMTP. Gmail filters would need the Gmail API, which is not supported yet.
- While the Google project's consent screen is in Testing, only allowed accounts can connect. Public distribution needs Google's approval for the mailbox permissions.
- A Microsoft organization can require administrator approval or block IMAP and SMTP AUTH. The app cannot override these policies.
