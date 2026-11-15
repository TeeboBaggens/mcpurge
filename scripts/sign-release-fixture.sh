#!/bin/sh
# Re-sign the test fixture after rotating the release signing key.
#
# The fixture is what proves the public key committed to this repo is the key
# that signs releases: if the two ever drift apart, the update verifier would
# reject every real release, and this is the test that says so first.
#
# Needs the release signing private key in the local GnuPG keyring. It lives in
# ~/protondrive/_backups/gpg/ alongside its passphrase; import it with
#   gpg --import mcpurge-release-signing-key.asc
set -eu

KEY="${MCPURGE_RELEASE_KEY:-E5FD1E0C7E53CC1815F435833BE10A2EF8137679}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

gpg --armor --detach-sign --local-user "$KEY" \
    --output "$ROOT/tests/fixtures/signed-message.txt.asc" \
    --yes "$ROOT/tests/fixtures/signed-message.txt"

gpg --armor --export "$KEY" > "$ROOT/assets/release-signing-key.asc"

echo "signed, and exported the public key to assets/release-signing-key.asc"
echo "update SIGNING_KEY_FINGERPRINT in src/update.rs if the key changed:"
gpg --list-keys --with-colons "$KEY" | awk -F: '/^fpr/{print $10; exit}'
