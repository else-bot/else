# Routines and webhooks

These are the planned triggers and authorization rules. The [security notes](security.md) cover prompt injection and the installation master key.

Routines should trigger a prompt in a bot conversation so it can carry out automated tasks.

They should support two kinds of triggers:

- A cron schedule for recurring tasks.
- An event, starting with webhooks authenticated through an `Authorization` header.

## Webhook authorization

Generate a separate bearer token for each webhook from 32 cryptographically random bytes, encoded as base64url. Generate each token independently of `SECRET_KEY` and every other webhook token. Enforce uniqueness on the stored token hash. The caller sends it over HTTPS:

```http
POST /webhooks/{webhook_id}
Authorization: Bearer <webhook_token>
```

The proposed storage is a SHA-256 hash of the token in PostgreSQL, alongside the webhook ID, associated bot and routine, and active or revoked state. Show the original token once when creating it. else.bot only needs to verify incoming tokens, so it can hash the submitted value and compare the digests in constant time. Recoverable encryption is for credentials the assistant must read back or send to another service. [Token digest and encryption patterns](https://docs.gitlab.com/development/authentication/).

Check the token against the webhook ID in the request. A leaked token authorizes only that webhook and its routine's permitted actions. Support revocation and replacement independently for each webhook. Restoring the database preserves token verification without requiring callers to change their tokens. Keep tokens out of URLs and redact the authorization header from logs. [Header handling](https://cheatsheetseries.owasp.org/cheatsheets/REST_Security_Cheat_Sheet.html).

Authentication lets the caller trigger the saved routine. Its payload remains external data. Validate payload fields against the routine's schema and pass them as inputs to its stored prompt. The payload cannot select another bot or routine, replace the system prompt, or expand permissions.
