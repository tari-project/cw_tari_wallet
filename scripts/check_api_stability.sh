#!/usr/bin/env bash
#
# Public-API stability tripwire.
#
# The codegen-drift job proves "generated == source"; this guard proves that an
# *intentional* breaking regeneration was not committed. It classifies the diff
# to the frozen Dart contract (`.dart/api/**`) between a base ref and HEAD:
#
#   * PASS  — purely additive changes (only `+` declaration lines), or no change.
#   * FAIL  — any removed/renamed declaration line (a `-` line that defines a
#             public symbol: function decl, class/enum name, field, or enum
#             variant). Renames and retypes show up as a removed line plus an
#             added line, so the removed side trips the wire.
#
# This is a pragmatic tripwire, not a formal API differ. It deliberately favours
# false positives (block, overridable) over false negatives (silent break).
#
# The block is overridable when the PR carries the `breaking-api-approved` label
# (CI passes OVERRIDE=true), so a coordinated, reviewed break is never *silent*.
#
# Usage: check_api_stability.sh <base-ref> [<head-ref>]
set -euo pipefail

BASE_REF="${1:?usage: check_api_stability.sh <base-ref> [<head-ref>]}"
HEAD_REF="${2:-HEAD}"
OVERRIDE="${OVERRIDE:-false}"

API_GLOB='.dart/api/'

# Lines under .dart/api/** that were *removed* by this change. `git diff` with a
# triple-dot range diffs against the merge-base, matching the merge semantics.
removed_lines="$(
  git diff "${BASE_REF}...${HEAD_REF}" -- "${API_GLOB}" \
    | grep -E '^-' \
    | grep -vE '^---' \
    || true
)"

if [ -z "${removed_lines}" ]; then
  echo "Public API stability: OK (no removals under .dart/api/**)."
  exit 0
fi

# A "declaration" line is one that introduces a public symbol consumed by Cake
# Wallet. We match on the structure of flutter_rust_bridge's generated Dart:
#   - top-level functions:        `<Type> name(...)` / `Future<...> name(...) =>`
#   - class / enum / typedef:     `class Foo {`, `enum Bar {`, `typedef ...`
#   - fields:                     `final <Type> name;`
#   - enum variants:              `  mainNet,`
# Comments, imports, braces, and `@override`/operator boilerplate are ignored.
declaration_removals="$(
  printf '%s\n' "${removed_lines}" \
    | sed -E 's/^-//' \
    | grep -vE '^\s*//' \
    | grep -vE "^\s*$" \
    | grep -E \
        -e '^\s*(class|enum|typedef|extension|mixin|sealed class|abstract class)\s+[A-Za-z_]' \
        -e '^\s*final\s+[A-Za-z_].*;' \
        -e '^\s*[A-Za-z_<][A-Za-z0-9_<>,?\. ]*\s+[a-z_][A-Za-z0-9_]*\s*\(' \
        -e '^\s*[a-z][A-Za-z0-9_]*\s*,\s*$' \
    || true
)"

if [ -z "${declaration_removals}" ]; then
  echo "Public API stability: OK (removals were non-declaration lines only)."
  exit 0
fi

echo "Detected removed/renamed public-API declaration line(s) under .dart/api/**:"
echo "----------------------------------------------------------------------"
printf '%s\n' "${declaration_removals}"
echo "----------------------------------------------------------------------"

if [ "${OVERRIDE}" = "true" ]; then
  echo "::warning::Breaking public-API change detected, but the 'breaking-api-approved' label is present — allowing."
  exit 0
fi

cat <<'MSG'
::error::This changes the public API consumed by Cake Wallet. Breaking changes are forbidden — additive only, or coordinate a migration.

If this break is intentional and has been coordinated with Cake Wallet, add the
'breaking-api-approved' label to the PR and re-run this check.
MSG
exit 1
