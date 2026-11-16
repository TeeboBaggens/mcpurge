#!/bin/sh
# Re-sign the test fixtures after rotating the release signing key.
#
# The fixtures are what prove the public key committed to this repo is the key
# that signs releases: if the two ever drift apart, the update verifier would
# reject every real release, and these are the tests that say so first.
#
#   signed-message.txt.minisig             made by the real release key
#   signed-message.txt.wrong-key.minisig   made by a throwaway key, so the
#                                          "refuses another key" test is
#                                          testing a real foreign signature
#   signed-message.txt.legacy.minisig      made by the real release key in
#                                          minisign's pre-hashing-era format,
#                                          which the verifier also refuses
#
# Needs the release secret key and its password, which live together in
# ~/protondrive/_backups/minisign/. Nothing is written to ~/.minisign.
set -eu

BACKUP="${MCPURGE_RELEASE_KEY_DIR:-$HOME/protondrive/_backups/minisign}"
SECRET="$BACKUP/mcpurge-release.key"
PASSWORD_FILE="$BACKUP/mcpurge-release-password.txt"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE="$ROOT/tests/fixtures/signed-message.txt"

for f in "$SECRET" "$PASSWORD_FILE"; do
    [ -r "$f" ] || { echo "cannot read $f" >&2; exit 1; }
done

head -1 "$PASSWORD_FILE" | minisign -S -s "$SECRET" -m "$FIXTURE" \
    -t "mcpurge test fixture, signed by the release key" >/dev/null

# The same file again in minisign's legacy (not pre-hashed) format. The
# verifier refuses it, and this is the signature that proves it does.
head -1 "$PASSWORD_FILE" | minisign -S -l -s "$SECRET" -m "$FIXTURE" \
    -x "$FIXTURE.legacy.minisig" -t "legacy format, which verify() refuses" >/dev/null

# The public key is recreated from the secret key rather than copied, so the
# committed key cannot be anything other than this key's public half.
head -1 "$PASSWORD_FILE" |
    minisign -R -s "$SECRET" -p "$ROOT/assets/release-signing-key.pub" -f >/dev/null

# A signature from a key that is not the release key. Generated fresh and
# thrown away: the test only cares that it is *someone else's*.
other="$(mktemp -d)"
trap 'rm -rf "$other"' EXIT
printf 'throwaway\nthrowaway\n' |
    minisign -G -p "$other/other.pub" -s "$other/other.key" >/dev/null
printf 'throwaway\n' | minisign -S -s "$other/other.key" -m "$FIXTURE" \
    -x "$FIXTURE.wrong-key.minisig" -t "not the release key" >/dev/null

minisign -V -p "$ROOT/assets/release-signing-key.pub" -m "$FIXTURE" >/dev/null
echo "signed, and recreated assets/release-signing-key.pub from the secret key"
echo "update SIGNING_KEY_ID in src/update.rs if the key changed:"
sed -n '1s/.*minisign public key //p' "$ROOT/assets/release-signing-key.pub"
