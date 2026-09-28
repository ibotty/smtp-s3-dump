# SMTP microservice to get mail and store in S3

This is a SMTP server meant to be deployed behind a real MTA (postfix) to accept mail for programmatic consumption.
It explodes mail to S3 with metadata, attachments (mime parts).

## automatic consumption of S3 data
It might emit a CloudEvent eventually, but for now use s3 bucket notifications.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `SMTP_BIND_ADDR` | `0.0.0.0:2525` | Listen address. |
| `MAX_SESSIONS` | `100` | Max concurrent SMTP sessions; extra connections are dropped. |
| `MAX_SESSIONS_PER_IP` | `10` | Max concurrent sessions per peer IP. Behind a proxy/MTA all clients share the proxy's IP, so raise this accordingly. |
| `ALLOWED_RCPTS` | unset | Comma-separated recipient allowlist. |
| `ALLOWED_FROMS` | unset | Comma-separated sender allowlist (see below). |
| `CHECK_ALLOWED_IN_DB` | `false` | Check allowed addresses in the database. |

Each session buffers the whole message (up to 100 MB) in memory, so size `MAX_SESSIONS` against available RAM.
If `ALLOWED_RCPTS`, `ALLOWED_FROMS` and `CHECK_ALLOWED_IN_DB` are all unset, anyone who can reach the port can
store mail in S3 and Postgres; a warning is logged at startup. Enable at least one in production.

## Security: `ALLOWED_FROMS` is not authentication

`MAIL FROM` is asserted by the client. This server performs no SMTP AUTH, SPF or DKIM verification, so
`ALLOWED_FROMS` is only a spam filter, not a security boundary. The stored `from` value is unauthenticated
and must not be trusted by downstream consumers. Do the authentication in the upstream MTA and restrict
network access to it.
