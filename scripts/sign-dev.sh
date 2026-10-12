#!/bin/bash
# Sign a dev build with a stable identity so the macOS Keychain's "Always
# Allow" sticks across rebuilds. Ad-hoc signatures change every build, and
# Keychain ACLs trust a binary's code requirement, so an unsigned dev build
# re-prompts for the saved password on every launch.
#
# Identity: $TUSK_SIGN_IDENTITY, else the first "Apple Development" one.
# The SHA-1 hash is extracted, not the name: names can hold non-ASCII
# ("Apple Development: Földi Dániel …"), and any encoding mangling between
# shells makes codesign miss the identity.
# No identity → leave the binary as is (the prompt just comes back).
set -euo pipefail
bin="$1"
id="${TUSK_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null \
  | awk '/Apple Development/ { print $2; exit }')}"
if [ -n "$id" ]; then
  # No 2>/dev/null: a failed codesign must say why.
  if ! codesign --force --sign "$id" --identifier ai.reyz.tusk "$bin"; then
    echo "sign-dev: codesign with $id failed, running unsigned" >&2
  fi
fi
