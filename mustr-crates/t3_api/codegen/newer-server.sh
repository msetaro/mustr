#!/usr/bin/env bash
# Record what a newer T3 Code sends, to prove this crate decodes it.
#
#   mustr-crates/t3_api/codegen/newer-server.sh <newer-t3-checkout> [--samples=N]
#
# Runs regen.sh on the newer checkout into a scratch directory (nothing here changes), then
# keeps only what a server sends: for every RPC, sample values of its success (or stream
# element) and error types, and for every HTTP endpoint, its responses. They go to
# tests/fixtures/newer-server/ with the newer manifest, and `tests/newer_server.rs` decodes each
# one with *this* build's types, by RPC tag and endpoint path. Values captured from a running
# newer server go in tests/fixtures/newer-server/live/ (see the README, "Forward compatibility").
#
# Env: MUSTR_T3_VERSION, MUSTR_T3_COMMIT   recorded in SOURCE.json (required)
#      MUSTR_T3_CODEGEN_WORK              work dir (default ${TMPDIR:-/tmp}/mustr-t3-newer-work)
# The checkout is untrusted, exactly as for regen.sh: nothing runs inside it.
set -euo pipefail

die() { echo "newer-server: $*" >&2; exit 1; }
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE="$(cd "$HERE/.." && pwd)"
T3_ARG="" SAMPLES=8
while [ $# -gt 0 ]; do
  case "$1" in
    --samples=*) SAMPLES="${1#--samples=}"; shift ;;
    -*) die "unknown option $1" ;;
    *) [ -z "$T3_ARG" ] || die "only one checkout"; T3_ARG="$1"; shift ;;
  esac
done
[ -n "$T3_ARG" ] || die "usage: $0 <newer-t3-checkout> [--samples=N]"
[ -n "${MUSTR_T3_VERSION:-}" ] || die "set MUSTR_T3_VERSION (e.g. 0.1.4-nightly.20261028.3841)"
[ -n "${MUSTR_T3_COMMIT:-}" ] || die "set MUSTR_T3_COMMIT"
command -v jq >/dev/null 2>&1 || die "jq not found"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/mustr-t3-newer.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
MUSTR_T3_CODEGEN_WORK="${MUSTR_T3_CODEGEN_WORK:-${TMPDIR:-/tmp}/mustr-t3-newer-work}" \
  "$HERE/regen.sh" "$T3_ARG" --out "$SCRATCH" --no-test --samples="$SAMPLES" >&2

DEST="$CRATE/tests/fixtures/newer-server"
mkdir -p "$DEST/samples" "$DEST/live"
rm -f "$DEST"/samples/*.json
# The RPC and HTTP surface only: tags, paths and the definitions a server sends.
jq '{
  rpc: [.rpc[] | {tag, stream, success: (if .success.kind == "ref" then .success.def else null end),
                  error: (if .error.kind == "ref" then .error.def else null end)}],
  http: [.http[] | {method, path,
                    success: ([.success[]? | .type | select(.kind == "ref") | .def] | first),
                    errors: [.errors[]? | .type | select(.kind == "ref") | .def] | unique}]
}' "$SCRATCH/schema/manifest.json" >"$DEST/manifest.json"
jq -r '[.rpc[] | .success, .error] + [.http[] | .success, .errors[]] | map(select(. != null)) | unique | .[]' \
  "$DEST/manifest.json" | while read -r def; do
  if [ -f "$SCRATCH/tests/fixtures/samples/$def.json" ]; then
    cp "$SCRATCH/tests/fixtures/samples/$def.json" "$DEST/samples/$def.json"
  fi
done
jq -n --arg version "$MUSTR_T3_VERSION" --arg commit "$MUSTR_T3_COMMIT" --argjson samples "$SAMPLES" \
  '{t3Version: $version, t3Commit: $commit, samplesPerType: $samples}' >"$DEST/SOURCE.json"
echo "newer-server: $(ls "$DEST/samples" | wc -l | tr -d ' ') sampled types from T3 $MUSTR_T3_VERSION -> $DEST" >&2
