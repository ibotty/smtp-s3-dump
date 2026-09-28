#!/usr/bin/env bash
# Integration test for examples/dump.rs, driven over a real TCP socket with `swaks`
# (https://www.jetmore.org/john/code/swaks/): STARTTLS, mail-parser part explosion, exact
# byte-for-byte roundtrip of csv/binary attachments, and recipient rejection. Run directly
# (`bash tests/swaks.sh`, from anywhere) or via `cargo test --test swaks`, which execs this
# script and skips it if `swaks` isn't installed.
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

host=127.0.0.1
port=2525
server_pid=
csv_src=
bin_src=

cleanup() {
    [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null
    [[ -n "$csv_src" ]] && rm -f "$csv_src"
    [[ -n "$bin_src" ]] && rm -f "$bin_src"
    rm -rf "alice@mx.example.org" "bob@mx.example.org" "carol@mx.example.org"
}
trap cleanup EXIT

echo "==> building examples/dump" >&2
bin=$(cargo build --quiet --example dump --all-features --message-format=json |
    grep -o '"executable":"[^"]*/dump"' | tail -1 | sed -E 's/^"executable":"(.*)"$/\1/')
[[ -x "$bin" ]] || { echo "could not locate the built 'dump' example binary" >&2; exit 1; }

"$bin" &
server_pid=$!

echo "==> waiting for $host:$port" >&2
for _ in $(seq 1 50); do
    { exec 3<>"/dev/tcp/$host/$port"; } 2>/dev/null && { exec 3>&-; break; }
    sleep 0.1
done
{ exec 3<>"/dev/tcp/$host/$port"; } 2>/dev/null || { echo "server never started listening" >&2; exit 1; }
exec 3>&-

echo "==> delivery with an attachment to alice@mx.example.org" >&2
swaks --server "$host:$port" --from sender@example.org --to alice@mx.example.org \
    --header "Subject: plain" --body "hello, plainly" \
    --attach-type text/plain --attach-name notes.txt --attach "attachment contents"
msgdir="alice@mx.example.org/$(ls "alice@mx.example.org")"
test -f "$msgdir/headers.txt"
test -f "$msgdir/body.txt"
test -f "$msgdir/attachments/00-notes.txt"

echo "==> STARTTLS delivery to bob@mx.example.org" >&2
swaks --server "$host:$port" -tls --from sender@example.org --to bob@mx.example.org \
    --header "Subject: over tls" --body "hello, encrypted"
msgdir="bob@mx.example.org/$(ls "bob@mx.example.org")"
test -f "$msgdir/headers.txt"
test -f "$msgdir/body.txt"

echo "==> attachment roundtrip (csv + binary) to carol@mx.example.org" >&2
csv_src=$(mktemp)
bin_src=$(mktemp)
printf 'name,qty,price\nwidget,3,9.99\ngadget,1,19.99\n' >"$csv_src"
printf '\x00\x01\x02\x03\xff\xfe\x80\x81\r\n\r\n\x7f' >"$bin_src"
swaks --server "$host:$port" --from sender@example.org --to carol@mx.example.org \
    --header "Subject: attachments" --body "see attached" \
    --attach-type text/csv --attach-name data.csv --attach "@$csv_src" \
    --attach-type application/octet-stream --attach-name blob.bin --attach "@$bin_src"
msgdir="carol@mx.example.org/$(ls "carol@mx.example.org")"
cmp -s "$csv_src" "$msgdir/attachments/00-data.csv" ||
    { echo "csv attachment roundtrip mismatch" >&2; exit 1; }
cmp -s "$bin_src" "$msgdir/attachments/01-blob.bin" ||
    { echo "binary attachment roundtrip mismatch" >&2; exit 1; }
rm -f "$csv_src" "$bin_src"

echo "==> rejecting a recipient in a domain we don't serve" >&2
if swaks --server "$host:$port" --from sender@example.org --to nobody@elsewhere.example \
    --header "Subject: should bounce" --body "nope"; then
    echo "expected swaks to fail: server should reject the recipient" >&2
    exit 1
fi
[[ ! -e elsewhere.example ]] || { echo "server stored mail for a rejected recipient" >&2; exit 1; }

echo "==> ok" >&2
