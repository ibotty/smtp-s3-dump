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
| `SMTP_PROXY_BIND_ADDR` | unset | Optional second listen address that requires a [PROXY protocol](https://www.haproxy.org/download/3.0/doc/proxy-protocol.txt) (v1 or v2) header on every connection. See below. |
| `PROXY_TRUSTED` | unset | Comma-separated CIDRs (or IPs) of the proxies allowed to connect to `SMTP_PROXY_BIND_ADDR`. Required when it is set. |
| `MAX_SESSIONS_PER_IP` | `10` | Max concurrent sessions per client IP: the PROXY header's source address on `SMTP_PROXY_BIND_ADDR`, the peer IP on `SMTP_BIND_ADDR`. |
| `ALLOWED_RCPTS` | unset | Comma-separated recipient allowlist. |
| `ALLOWED_FROMS` | unset | Comma-separated sender allowlist (see below). |
| `CHECK_ALLOWED_IN_DB` | `false` | Check allowed addresses in the database. |

Each session buffers the whole message (up to 100 MB) in memory, so size `MAX_SESSIONS` against available RAM.
If `ALLOWED_RCPTS`, `ALLOWED_FROMS` and `CHECK_ALLOWED_IN_DB` are all unset, anyone who can reach the port can
store mail in S3 and Postgres; a warning is logged at startup. Enable at least one in production.

## Behind a load balancer (PROXY protocol)

`SMTP_BIND_ADDR` never speaks PROXY, so clients cannot spoof their address there. To see real client
addresses behind HAProxy, nginx, etc., set `SMTP_PROXY_BIND_ADDR` and point the proxy at it (for example
`send-proxy-v2`), and list the proxies in `PROXY_TRUSTED`. Connections to that port from other peers, and
connections without a valid header within 5 seconds, are dropped (logged, no SMTP reply). Do not expose it
to untrusted networks: a trusted proxy is believed about the client address.

The PROXY header is read before TLS and SMTP, so `STARTTLS` is unaffected. `LOCAL`/`UNKNOWN` headers keep the
proxy's address. The v1 parser needs at least 32 bytes, so a bare `PROXY UNKNOWN` health-check line stalls and is
dropped after the timeout; use v2. Invalid headers are also logged by the parser at error level; silence that
with `RUST_LOG=...,haproxy_protocol=off`.

## Security: `ALLOWED_FROMS` is not authentication

`MAIL FROM` is asserted by the client. This server performs no SMTP AUTH, SPF or DKIM verification, so
`ALLOWED_FROMS` is only a spam filter, not a security boundary. The stored `from` value is unauthenticated
and must not be trusted by downstream consumers. Do the authentication in the upstream MTA and restrict
network access to it.
