#!/bin/sh
set -eu

fail() {
    printf 'macOS signing verification failed: %s\n' "$*" >&2
    exit 1
}

[ "$#" -eq 4 ] || fail 'expected <binary> <identity> <team-id> <identifier>'

binary=$1
identity=$2
team_id=$3
identifier=$4

[ -f "$binary" ] || fail "binary does not exist: $binary"
[ -n "$identity" ] || fail 'signing identity is empty'
[ -n "$team_id" ] || fail 'Team ID is empty'
[ -n "$identifier" ] || fail 'identifier is empty'

set -- --force
if [ -n "${CODE_SIGN_KEYCHAIN:-}" ]; then
    set -- "$@" --keychain "$CODE_SIGN_KEYCHAIN"
fi
set -- \
    "$@" \
    --sign "$identity" \
    --identifier "$identifier" \
    --options runtime \
    --timestamp
codesign "$@" "$binary"

codesign --verify --strict --verbose=2 "$binary"

details=$(codesign --display --verbose=4 "$binary" 2>&1)
printf '%s\n' "$details" | grep -Fqx "Identifier=$identifier" \
    || fail "Identifier does not equal $identifier"
printf '%s\n' "$details" | grep -Fqx "TeamIdentifier=$team_id" \
    || fail "TeamIdentifier does not equal $team_id"

requirement=$(codesign --display --requirements - "$binary" 2>&1)
printf '%s\n' "$requirement" | grep -Fq "identifier \"$identifier\"" \
    || fail "designated requirement does not contain identifier $identifier"
printf '%s\n' "$requirement" | grep -Fq 'anchor apple generic' \
    || fail 'designated requirement does not use the Apple generic anchor'
printf '%s\n' "$requirement" | grep -Fq "$team_id" \
    || fail "designated requirement does not contain Team ID $team_id"
