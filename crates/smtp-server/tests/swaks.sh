#!/usr/bin/env bash
# Integration test for examples/dump.rs, driven over a real TCP socket with `swaks`
# (https://www.jetmore.org/john/code/swaks/): STARTTLS, implicit TLS, mail-parser part explosion, exact
# byte-for-byte roundtrip of csv/binary attachments, recipient rejection, and PROXY protocol v1/v2
# (swaks' --proxy-*) on the PROXY listener. Run directly
# (`bash tests/swaks.sh`, from anywhere) or via `cargo test --test swaks`, which execs this
# script and skips it if `swaks` isn't installed.
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

host=127.0.0.1
port=2525
tls_port=4650
proxy_port=2526
server_pid=
tmp=

cleanup() {
    [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null
    [[ -n "$tmp" ]] && rm -rf "$tmp"
    rm -rf ./*@mx.example.org
}
trap cleanup EXIT

tmp=$(mktemp -d)

# Runs one test function in a subshell (with `set -e`), cargo-test style: output is shown only on failure.
passed=0
failed=0
failures=()
run_test() {
    local name=$1 out rc
    printf 'test swaks::%s ... ' "$name"
    set +e
    out=$(set -e; "$name" 2>&1)
    rc=$?
    set -e
    if ((rc == 0)); then
        echo ok
        passed=$((passed + 1))
    else
        echo FAILED
        failed=$((failed + 1))
        failures+=("$name")
        printf '\n---- swaks::%s ----\n%s\n\n' "$name" "$out"
    fi
}

bin=$(cargo build --quiet --example dump --all-features --message-format=json |
    grep -o '"executable":"[^"]*/dump"' | tail -1 | sed -E 's/^"executable":"(.*)"$/\1/')
[[ -x "$bin" ]] || { echo "could not locate the built 'dump' example binary" >&2; exit 1; }

"$bin" >"$tmp/server.log" 2>&1 &
server_pid=$!

# Only probes the port: the implicit-TLS and PROXY listeners just see a connect + close.
for p in "$port" "$tls_port" "$proxy_port"; do
    for _ in $(seq 1 50); do
        { exec 3<>"/dev/tcp/$host/$p"; } 2>/dev/null && { exec 3>&-; break; }
        sleep 0.1
    done
    { exec 3<>"/dev/tcp/$host/$p"; } 2>/dev/null || { echo "server never started listening on $p" >&2; cat "$tmp/server.log" >&2; exit 1; }
    exec 3>&-
done

swaks_to() { # <port> <recipient> [swaks args...]
    local p=$1 to=$2
    shift 2
    swaks --server "$host:$p" --from sender@example.org --to "$to" "$@"
}

delivery_with_attachment() {
    swaks_to "$port" alice@mx.example.org \
        --header "Subject: plain" --body "hello, plainly" \
        --attach-type text/plain --attach-name notes.txt --attach "attachment contents"
    msgdir="alice@mx.example.org/$(ls "alice@mx.example.org")"
    test -f "$msgdir/headers.txt"
    test -f "$msgdir/body.txt"
    test -f "$msgdir/attachments/00-notes.txt"
}

starttls_delivery() {
    swaks_to "$port" bob@mx.example.org -tls \
        --header "Subject: over tls" --body "hello, encrypted"
    msgdir="bob@mx.example.org/$(ls "bob@mx.example.org")"
    test -f "$msgdir/headers.txt"
    test -f "$msgdir/body.txt"
}

implicit_tls_delivery() {
    swaks_to "$tls_port" dave@mx.example.org --tls-on-connect \
        --header "Subject: over implicit tls" --body "hello, encrypted from the first byte"
    msgdir="dave@mx.example.org/$(ls "dave@mx.example.org")"
    test -f "$msgdir/headers.txt"
    test -f "$msgdir/body.txt"
}

attachment_roundtrip() {
    local csv_src="$tmp/data.csv" bin_src="$tmp/blob.bin"
    printf 'name,qty,price\nwidget,3,9.99\ngadget,1,19.99\n' >"$csv_src"
    printf '\x00\x01\x02\x03\xff\xfe\x80\x81\r\n\r\n\x7f' >"$bin_src"
    swaks_to "$port" carol@mx.example.org \
        --header "Subject: attachments" --body "see attached" \
        --attach-type text/csv --attach-name data.csv --attach "@$csv_src" \
        --attach-type application/octet-stream --attach-name blob.bin --attach "@$bin_src"
    msgdir="carol@mx.example.org/$(ls "carol@mx.example.org")"
    cmp -s "$csv_src" "$msgdir/attachments/00-data.csv" || { echo "csv attachment roundtrip mismatch"; return 1; }
    cmp -s "$bin_src" "$msgdir/attachments/01-blob.bin" || { echo "binary attachment roundtrip mismatch"; return 1; }
}

reject_foreign_recipient() {
    if swaks_to "$port" nobody@elsewhere.example --header "Subject: should bounce" --body "nope" >/dev/null 2>&1; then
        echo "expected swaks to fail: server should reject the recipient"
        return 1
    fi
    [[ ! -e elsewhere.example ]] || { echo "server stored mail for a rejected recipient"; return 1; }
}

# The client address the server recorded for the (single) message stored for <recipient>.
peer_of() {
    cat "$1/$(ls "$1")/peer.txt"
}

proxy_v1_delivery() {
    swaks_to "$proxy_port" proxy1@mx.example.org \
        --proxy-version 1 --proxy-family TCP4 --proxy-source 203.0.113.9 --proxy-source-port 5555 \
        --proxy-dest 198.51.100.1 --proxy-dest-port 25 \
        --header "Subject: via proxy v1" --body "hello through a proxy"
    [[ $(peer_of proxy1@mx.example.org) == "203.0.113.9:5555" ]] || { echo "wrong peer: $(peer_of proxy1@mx.example.org)"; return 1; }
}

proxy_v2_starttls_delivery() {
    swaks_to "$proxy_port" proxy2@mx.example.org -tls \
        --proxy-version 2 --proxy-family AF_INET6 --proxy-source 2001:db8::9 --proxy-source-port 5555 \
        --proxy-dest 2001:db8::1 --proxy-dest-port 25 \
        --header "Subject: via proxy v2" --body "hello through a proxy, encrypted"
    [[ $(peer_of proxy2@mx.example.org) == "[2001:db8::9]:5555" ]] || { echo "wrong peer: $(peer_of proxy2@mx.example.org)"; return 1; }
}

# swaks' own v2 LOCAL is truncated, so send the raw bytes after the signature: 0x20 = v2 LOCAL,
# 0x00 = AF_UNSPEC, 0x0000 = no payload.
proxy_v2_local_keeps_tcp_peer() {
    printf '\x20\x00\x00\x00' >"$tmp/local.bin"
    swaks_to "$proxy_port" proxy3@mx.example.org \
        --proxy-version 2 --proxy "@$tmp/local.bin" \
        --header "Subject: via proxy v2 local" --body "health check style"
    [[ $(peer_of proxy3@mx.example.org) =~ ^127\.0\.0\.1:[0-9]+$ ]] || { echo "wrong peer: $(peer_of proxy3@mx.example.org)"; return 1; }
}

proxy_port_requires_header() {
    if swaks_to "$proxy_port" proxy4@mx.example.org --timeout 10 --header "Subject: no header" --body "nope" >/dev/null 2>&1; then
        echo "expected swaks to fail: the PROXY listener needs a PROXY header"
        return 1
    fi
    [[ ! -e proxy4@mx.example.org ]] || { echo "server stored mail without a PROXY header"; return 1; }
}

# The plain listener must never believe a PROXY header (it would be spoofable): either it refuses
# the session or the recorded peer is the real one.
plain_port_ignores_proxy_header() {
    swaks_to "$port" spoof@mx.example.org --timeout 10 \
        --proxy-version 1 --proxy-family TCP4 --proxy-source 203.0.113.66 --proxy-source-port 5555 \
        --proxy-dest 198.51.100.1 --proxy-dest-port 25 \
        --header "Subject: spoof" --body "nope" >/dev/null 2>&1 || true
    if [[ -d spoof@mx.example.org ]] && [[ $(peer_of spoof@mx.example.org) == 203.0.113.66:* ]]; then
        echo "plain listener believed a PROXY header"
        return 1
    fi
}

tests=(delivery_with_attachment starttls_delivery implicit_tls_delivery attachment_roundtrip reject_foreign_recipient
    proxy_v1_delivery proxy_v2_starttls_delivery proxy_v2_local_keeps_tcp_peer proxy_port_requires_header
    plain_port_ignores_proxy_header)
printf '\nrunning %d tests\n' "${#tests[@]}"
for t in "${tests[@]}"; do run_test "$t"; done

if ((failed > 0)); then
    printf '\n---- server log ----\n'
    cat "$tmp/server.log"
    printf '\nfailures:\n'
    printf '    swaks::%s\n' "${failures[@]}"
    printf '\ntest result: FAILED. %d passed; %d failed\n' "$passed" "$failed"
    exit 1
fi
printf '\ntest result: ok. %d passed; %d failed\n' "$passed" "$failed"
