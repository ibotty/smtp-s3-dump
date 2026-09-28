#!/usr/bin/env bash
# Integration test for examples/dump.rs, driven over a real TCP socket with `swaks`
# (https://www.jetmore.org/john/code/swaks/): STARTTLS, implicit TLS, mail-parser part explosion, exact
# byte-for-byte roundtrip of csv/binary attachments, and recipient rejection. Run directly
# (`bash tests/swaks.sh`, from anywhere) or via `cargo test --test swaks`, which execs this
# script and skips it if `swaks` isn't installed.
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

host=127.0.0.1
port=2525
tls_port=4650
server_pid=
tmp=

cleanup() {
    [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null
    [[ -n "$tmp" ]] && rm -rf "$tmp"
    rm -rf "alice@mx.example.org" "bob@mx.example.org" "carol@mx.example.org" "dave@mx.example.org"
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

# Only probes the port: the implicit-TLS listener just sees a connect + close (failed handshake).
for p in "$port" "$tls_port"; do
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

tests=(delivery_with_attachment starttls_delivery implicit_tls_delivery attachment_roundtrip reject_foreign_recipient)
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
