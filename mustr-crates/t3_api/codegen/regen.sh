#!/usr/bin/env bash
# Regenerate the `t3_api` crate from a T3 Code checkout.
#
#   mustr-crates/t3_api/codegen/regen.sh <t3-checkout> [--out DIR] [--no-test] [--samples=N]
#
# --out DIR    Where to write schema/, src/generated/ and tests/fixtures/{samples,t3-tests-canonical}/.
#              Default: the t3_api crate this script lives in. Tests only run when DIR is a crate.
# --no-test    Skip `cargo test -p t3_api` (the round-trip tests).
# --samples=N  Arbitrary-generated wire values per root type (default 3).
#
# Pipeline: copy T3's packages/contracts (untrusted) to a work dir -> install the exact
# Effect version T3 pins (--ignore-scripts) -> dump JSON Schema + manifest with Node's
# built-in TS type stripping under Node's permission model -> typify-gen -> cargo test.
# Nothing runs inside the T3 checkout, and git is never invoked there.
#
# Env: MUSTR_T3_CODEGEN_WORK   work dir (default ${TMPDIR:-/tmp}/mustr-t3-codegen-work);
#                              holds the copied contracts and node_modules/effect
#      MUSTR_T3_CODEGEN_TARGET target dir for typify-gen (default codegen/typify-gen/target,
#                              so regen never waits on the Zed workspace's build lock)
#      MUSTR_T3_VERSION / MUSTR_T3_COMMIT   recorded in schema/SOURCE.json (set by
#                              script/mustr-sync-t3)
#      MUSTR_T3_EFFECT_VERSION if set, must equal the Effect version T3's catalog pins
#      CARGO, NODE, NPM
set -euo pipefail

usage() { echo "usage: $0 <t3-checkout> [--out DIR] [--no-test] [--samples=N]" >&2; exit 2; }
die() { echo "regen: $*" >&2; exit 1; }
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_SRC="$(cd "$HERE/.." && pwd)"   # the checked-in t3_api crate (hand-written parts, T3 test fixtures)

T3_ARG="" OUT="" RUN_TESTS=1 SAMPLES=3
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="${2:?--out needs a directory}"; shift 2 ;;
    --out=*) OUT="${1#--out=}"; shift ;;
    --no-test) RUN_TESTS=0; shift ;;
    --samples=*) SAMPLES="${1#--samples=}"; shift ;;
    -h|--help) usage ;;
    -*) usage ;;
    *) [ -z "$T3_ARG" ] || usage; T3_ARG="$1"; shift ;;
  esac
done
[ -n "$T3_ARG" ] || usage
T3="$(cd "$T3_ARG" && pwd)" || die "no such directory: $T3_ARG"
OUT="${OUT:-$CRATE_SRC}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

NODE="${NODE:-node}"
CARGO="${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}"
WORK="${MUSTR_T3_CODEGEN_WORK:-${TMPDIR:-/tmp}/mustr-t3-codegen-work}"
NPM="${NPM:-npm}"
GEN_TARGET="${MUSTR_T3_CODEGEN_TARGET:-$HERE/typify-gen/target}"
SCHEMA="$OUT/schema"
FIXTURES_IN="$CRATE_SRC/tests/fixtures/t3-tests"   # copied from T3's contract tests; inputs
FIXTURES_OUT="$OUT/tests/fixtures"
GEN="$HERE/typify-gen"

now_ms() { "$NODE" -e 'process.stdout.write(String(Date.now()))'; }
T_START=$(now_ms)
T_LAST=$T_START
step() { local t; t=$(now_ms); printf '  [%6d ms] %s\n' "$((t - T_LAST))" "$1"; T_LAST=$t; }

[ -f "$T3/packages/contracts/src/rpc.ts" ] || die "$T3 does not look like a T3 Code checkout"
"$NODE" -e 'const [a,b]=process.versions.node.split(".").map(Number); if (a < 23 && !(a === 22 && b >= 18)) process.exit(1)' \
  || die "node >= 22.18 needed (built-in TypeScript type stripping)"

# T3 commit without running git inside an untrusted repo (hooks/fsmonitor).
T3_COMMIT="${MUSTR_T3_COMMIT:-unknown}"
if [ "$T3_COMMIT" = unknown ] && [ -f "$T3/.git/HEAD" ]; then
  head_ref=$(cat "$T3/.git/HEAD")
  case "$head_ref" in
    ref:*) ref=${head_ref#ref: }
      if [ -f "$T3/.git/$ref" ]; then T3_COMMIT=$(cat "$T3/.git/$ref")
      else T3_COMMIT=$(grep " $ref\$" "$T3/.git/packed-refs" 2>/dev/null | cut -d' ' -f1 || echo unknown); fi ;;
    *) T3_COMMIT=$head_ref ;;
  esac
fi
T3_VERSION="${MUSTR_T3_VERSION:-unknown}"

# 1. Effect version: T3 pins it in the pnpm catalog. The schema->JSON Schema compiler lives
#    in Effect, so it must match exactly.
EFFECT_VERSION=$(sed -n 's/^  effect: *"\{0,1\}\([0-9][^" ]*\)"\{0,1\} *$/\1/p' "$T3/pnpm-workspace.yaml" | head -1)
[ -n "$EFFECT_VERSION" ] || die "could not read 'effect:' from the catalog in pnpm-workspace.yaml"
if [ -n "${MUSTR_T3_EFFECT_VERSION:-}" ] && [ "$MUSTR_T3_EFFECT_VERSION" != "$EFFECT_VERSION" ]; then
  die "caller expects effect@$MUSTR_T3_EFFECT_VERSION but T3's catalog pins effect@$EFFECT_VERSION"
fi
echo "regen: T3 $T3_VERSION (${T3_COMMIT:0:12}), effect@$EFFECT_VERSION, work dir $WORK, out $OUT"

# 2. The contracts package must only depend on effect; anything else needs a pipeline change.
"$NODE" -e '
  const p = require(process.argv[1]);
  const deps = Object.keys(p.dependencies ?? {});
  if (deps.length !== 1 || deps[0] !== "effect") { console.error("contracts deps changed:", deps); process.exit(1); }
' "$T3/packages/contracts/package.json" || die "contracts dependencies changed; update regen.sh"

# 3. Copy contracts into the work dir (we never execute anything in the T3 checkout itself).
mkdir -p "$WORK"
rm -rf "$WORK/contracts" "$WORK/run" "$WORK/out"
mkdir -p "$WORK/contracts" "$WORK/run" "$WORK/out"
cp -R "$T3/packages/contracts/src" "$WORK/contracts/src"
cp "$T3/packages/contracts/package.json" "$WORK/contracts/package.json"
cp "$HERE/dump-schemas.ts" "$WORK/run/dump-schemas.ts"
step "copied contracts ($(ls "$WORK/contracts/src" | wc -l | tr -d ' ') files)"

# 4. Install effect@EFFECT_VERSION once per version (no lifecycle scripts).
installed=$("$NODE" -p 'try { require(process.argv[1]).version } catch { "" }' "$WORK/node_modules/effect/package.json" 2>/dev/null || true)
if [ "$installed" != "$EFFECT_VERSION" ]; then
  [ -f "$WORK/package.json" ] || echo '{"name":"t3-codegen-work","private":true,"type":"module"}' > "$WORK/package.json"
  "$NPM" --prefix "$WORK" install --ignore-scripts --no-audit --no-fund --save-exact "effect@$EFFECT_VERSION" >/dev/null
  step "installed effect@$EFFECT_VERSION"
else
  step "effect@$EFFECT_VERSION already installed"
fi

# 5. Effect Schema -> JSON Schema. Node's permission model: fs reads limited to the work dir
#    and our fixtures, writes to work/out, no child processes / workers / addons.
"$NODE" --permission \
  --allow-fs-read="$WORK" --allow-fs-read="$FIXTURES_IN" --allow-fs-write="$WORK/out" \
  "$WORK/run/dump-schemas.ts" "$WORK/contracts/src" "$WORK/out" \
  --samples="$SAMPLES" --fixtures="$FIXTURES_IN"
step "dumped JSON Schema"

# 6. Publish the language-neutral artifacts (commit these: diffing them shows API changes).
mkdir -p "$SCHEMA" "$FIXTURES_OUT"
for f in t3-api.raw.schema.json t3-api.schema.json manifest.json openapi.json stats.json fixtures-check.json samples-failures.json; do
  rm -f "$SCHEMA/$f"
  [ -f "$WORK/out/$f" ] && cp "$WORK/out/$f" "$SCHEMA/$f"
done
printf '{\n  "t3Version": "%s",\n  "t3Commit": "%s",\n  "effectVersion": "%s",\n  "samplesPerType": %s\n}\n' \
  "$T3_VERSION" "$T3_COMMIT" "$EFFECT_VERSION" "$SAMPLES" > "$SCHEMA/SOURCE.json"
rm -rf "$FIXTURES_OUT/samples" "$FIXTURES_OUT/t3-tests-canonical"
[ -d "$WORK/out/samples" ] && cp -R "$WORK/out/samples" "$FIXTURES_OUT/samples"
[ -d "$WORK/out/fixtures-canonical" ] && cp -R "$WORK/out/fixtures-canonical" "$FIXTURES_OUT/t3-tests-canonical"
step "published schema/ and test corpus"

# 7. JSON Schema -> Rust (typify + our tagged-union/forward-compat/method-table emitter).
#    typify-gen is its own workspace (empty [workspace] table), pinned by its Cargo.lock.
CARGO_TARGET_DIR="$GEN_TARGET" "$CARGO" build --quiet --release --locked --manifest-path "$GEN/Cargo.toml"
step "built typify-gen"
mkdir -p "$OUT/src/generated"
"$GEN_TARGET/release/t3-typify-gen" "$SCHEMA" "$OUT/src/generated"
step "generated Rust"

# 8. Compile + round-trip every sampled wire value through the generated types.
if [ "$RUN_TESTS" = 1 ] && [ -f "$OUT/Cargo.toml" ]; then
  if ! test_out=$("$CARGO" test --quiet --manifest-path "$OUT/Cargo.toml" -- --nocapture 2>&1); then
    echo "$test_out" | tail -40
    die "cargo test failed (rerun: cargo test --manifest-path $OUT/Cargo.toml -- --nocapture)"
  fi
  echo "$test_out" | grep -E "samples: |newer-server|future-|test result" | sed 's/^/  /' || true
  step "cargo test"
elif [ "$RUN_TESTS" = 1 ]; then
  echo "regen: $OUT has no Cargo.toml; skipping tests"
fi

sizes=$(cat "$OUT"/src/generated/*.rs | wc -c | tr -d ' ')
lines=$(cat "$OUT"/src/generated/*.rs | wc -l | tr -d ' ')
echo "regen: done in $(( $(now_ms) - T_START )) ms; generated Rust: $lines lines, $sizes bytes"
