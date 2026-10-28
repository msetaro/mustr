# t3_api codegen

Generates the `t3_api` crate's wire types from T3 Code's Effect Schema contracts (`packages/contracts`). The generated code, `schema/`, and the test corpus are checked in, so building mustr needs neither node nor a T3 checkout. Only a T3 sync runs this.

```sh
git clone --depth 1 --branch v0.1.1 https://github.com/pingdotgg/t3code /tmp/t3code-v0.1.1
MUSTR_T3_VERSION=0.1.1 mustr-crates/t3_api/codegen/regen.sh /tmp/t3code-v0.1.1
```

`script/mustr-sync-t3` runs it for you (see `mustr-docs/SYNC.md`).

| Flag | Meaning |
|---|---|
| `--out DIR` | Write `schema/`, `src/generated/`, and `tests/fixtures/{samples,t3-tests-canonical}/` into DIR. Default: this crate. Tests only run when DIR has a `Cargo.toml`. |
| `--no-test` | Skip `cargo test` on the output crate |
| `--samples=N` | Arbitrary-generated wire values per root type (default 3) |

| Env | Meaning |
|---|---|
| `MUSTR_T3_CODEGEN_WORK` | Work dir: copied contracts, `node_modules/effect`, raw output. Default `${TMPDIR:-/tmp}/mustr-t3-codegen-work` |
| `MUSTR_T3_CODEGEN_TARGET` | Cargo target dir for typify-gen. Default `typify-gen/target` (gitignored), so regen never waits on the Zed workspace's build lock |
| `MUSTR_T3_VERSION`, `MUSTR_T3_COMMIT` | Recorded in `schema/SOURCE.json`. The commit defaults to the checkout's `.git/HEAD`, read without running git |
| `MUSTR_T3_EFFECT_VERSION` | If set, must equal the Effect version T3's catalog pins |
| `NODE`, `NPM`, `CARGO` | Tool overrides. Node 22.18 or later (built-in TypeScript type stripping) |

**Requirements:** node, npm, and cargo. npm needs the network once per Effect version.

## Pipeline

T3 `packages/contracts/src` → (copy into the work dir) → `node dump-schemas.ts` → JSON Schema + `manifest.json` → `typify-gen` → `src/generated/` → `cargo test`.

1. **`regen.sh`.** Reads the Effect version from T3's pnpm catalog (`pnpm-workspace.yaml`) and checks that `contracts` depends only on `effect`. It copies `contracts/src` into the work dir and installs exactly that Effect version with `npm install --ignore-scripts`. Effect 4 has no dependencies. The T3 checkout is untrusted: nothing runs inside it, and git is never invoked there.
2. **`dump-schemas.ts`** runs on plain `node` under `node --permission`: reads only from the work dir and the fixture inputs, writes only to the work dir, and can't spawn processes or workers.
   - Imports every contracts module and walks `WsRpcGroup.requests`: payload, success (or the stream element), and error. The error is the RPC's error ∪ stream error ∪ middleware errors, mirroring `Rpc.exitSchema`.
   - Covers HTTP with `HttpApi.reflect(EnvironmentHttpApi)`.
   - Feeds every root into one `SchemaRepresentation`, so definitions are shared. It emits JSON Schema with `toJsonSchemaMultiDocument`, then converts to draft-07 for typify.
   - **Why this is the wire format:** T3's server uses `RpcSerialization.layerJson`, whose codec is `Schema.toCodecJson`. `Schema.toJsonSchemaDocument` describes exactly the encoded side of that codec.
   - Also detects T3's forward-compatible fields (below), normalizes the schema for typify, writes a sample corpus with Effect's `Arbitrary`, and checks the T3 test fixtures with T3's own codec.
3. **`typify-gen/`** is a Rust tool: typify 0.8 as a library, then a syn pass (`postprocess.rs`), printed with prettyplease. It is its own cargo workspace (an empty `[workspace]` table, pinned by its `Cargo.lock`) and is not part of Zed's workspace. It writes:
   - `types.rs`: every wire type.
   - `tagged.rs`: discriminated unions, as `crate::tagged_union!` invocations.
   - `methods.rs`: `rpc::*` (one struct per RPC implementing `RpcMethod { TAG, STREAM, Payload, Success, Error }`), `http::*` (one per endpoint implementing `HttpEndpoint { METHOD, PATH, REQUEST_CONTENT_TYPE, CONSTANT_HEADERS, Request, Response, … }`), and `named::*`. `CONSTANT_HEADERS` lists the required headers with a fixed value, such as `x-t3-orchestration-protocol: 2` on the orchestration snapshot endpoints.
   - `dispatch.rs`: definition name → round-trip function, used only by `tests/roundtrip.rs`.
4. **Hand-written, depends only on Effect, not on T3:** `src/t3_api.rs` (the traits, `JsNumber`, `Never`, and the `string_enum!`/`tagged_union!` macros), `src/forward_compat.rs`, and `src/envelope.rs`. `envelope.rs` holds the Effect RPC frames (`Request`/`Chunk`/`Exit`/`Cause`…), which are TS interfaces in Effect, not schemas, so they can't be generated.

**Outputs to commit:** `schema/` (raw and normalized JSON Schema, `manifest.json`, `openapi.json`, `stats.json`, `rust-stats.json`, `SOURCE.json`), `src/generated/`, `tests/fixtures/samples/`, and `tests/fixtures/t3-tests-canonical/`. `git diff schema/manifest.json` is the API changelog.

**Inputs** in `tests/fixtures/t3-tests/` are copied by hand from T3's contract tests (`make-fixtures.mjs` records the source). regen.sh decodes each one with T3's codec and writes T3's own re-encoding to `t3-tests-canonical/`, which is the expected Rust output. A fixture T3 no longer accepts fails the regen.

## Forward compatibility

An older mustr must keep working against a newer T3 server. T3 evolves its schemas additively, but JSON Schema can't express T3's lenient decoding. So the Rust side handles it in four ways:

1. **String enums** with two or more values become `crate::string_enum!` invocations: an `Unrecognized(String)` variant, which serializes back verbatim, plus hand-written serde impls. typify's `Display`/`FromStr`/`TryFrom` impls are replaced by infallible ones. Single-value literals stay strict: they are constants such as discriminators, OAuth `grant_type`s, and the protocol header, and T3 rejects other values too.
2. **Discriminated unions** (416) become `crate::tagged_union!` enums: one newtype variant per tag value, plus `Unrecognized(serde_json::Value)`, which keeps the raw value.
   - dump-schemas hoists every discriminated union into a definition, including nested ones and nullable ones (`oneOf [oneOf [A, B], null]`).
   - The emitter refuses any serde-tagged enum typify still produces, because it would have no fallback. Hoist it instead.
   - T3's `ForwardCompatibleUnion` has a catch-all member (`{ [tag]: string }`). That member is folded into `Unrecognized`, and the union is marked `open = true`.
3. **T3's lenient fields** are detected in dump-schemas.ts (step 3b) and listed in `manifest.json` under `forwardCompat` as `{ def, property, kind, how }`. This covers `ForwardCompatibleArray`, `ForwardCompatibleUnionArray`, `ForwardCompatibleOptional`, `ForwardCompatibleNullable`, and `OmittedWhenNull`.
   - **Detection is behavioral.** Each property is decoded with T3's own schema on probe values: an array holding an unknown symbol or an unknown tag, or a bare unknown value. Whatever T3 drops or turns into `undefined`/`null` is lenient. So it depends neither on helper names nor on Effect internals.
   - The optional helpers encode as `Unknown` (`{}`). For those, the real inner type is spliced into the JSON Schema, and the field is typed rather than `serde_json::Value`.
   - `postprocess.rs` gives each listed field a `#[serde(default, deserialize_with = "crate::forward_compat::…")]` decoder, chosen by `kind` and the Rust field type:

     | Decoder | For | Behavior |
     |---|---|---|
     | `vec`, `option_vec` | lenient arrays | Each element is decoded strictly; elements that fail are dropped |
     | `known_vec`, `option_known_vec` | lenient arrays of tagged unions | Elements with an unknown tag are dropped too, as T3 does |
     | `option` | lenient optionals | A value that fails becomes `None` |

   - "Strictly" means with every `Unrecognized` fallback disabled, except in open unions. That is exactly T3's decoding of the element, so Rust drops what T3 drops.
   - A missing lenient array decodes as empty.
4. **Untagged unions** (44, with no discriminator) get `#[serde(remote = "Self")]` and a `Deserialize` that tries a strict pass first, then a lenient one. Otherwise an early variant's fallback would capture a value a later variant matches exactly, for example `KeybindingCommand`: a known command, or any string.

**Strict mode** is a thread-local flag (`forward_compat::strictly`). serde decoding is synchronous, so it can't leak across tasks.

**Tests:**
- `tests/forward_compat.rs` covers each mechanism.
- In `tests/roundtrip.rs`, `t3_test_fixtures` includes T3's `PullRequestDetail.newer-server` and `ExecutionEnvironmentDescriptor.future-manager`. Rust must drop exactly what T3 drops.
- Samples that T3's own decode→encode changes carry the T3 result in `<Def>.expected.json`.

**Not covered:**
- Required fields that T3 removes or renames. These are breaking changes; T3 bumps the orchestration protocol for those.
- New variants of untagged unions that no existing variant accepts.
- The opt-in compact `ids` table of `withRecordIdTable` responses (bounded snapshots, `subscribeThread`, history pages). It is not modelled, so mustr must not send `acceptCompactIds`, nor other `accept*` opt-ins until they are reviewed.

## Workarounds

Every workaround is counted in `schema/stats.json` (dump-schemas) and `schema/rust-stats.json` (typify-gen).

- **Names.** Effect only creates definitions for schemas with an `identifier`, and T3 rarely sets one.
  - A `referencePolicy` maps each export's memoized encoded AST node back to its TS export name, and identical duplicates are merged. Aliases lose to the original name.
  - It uses `SchemaAST.getContextOwner`/`getLastEncoding`, which are exported but `@internal`. **This is the most Effect-version-sensitive part.**
- **`withRecordIdTable`** is an opaque `Schema.declare` whose JSON Schema is `{}`. The last encoding link is dropped to get the real payload.
- **Unions:**
  - `anyOf` becomes `oneOf`.
  - Nested and `$ref`'d unions are flattened and deduplicated.
  - Discriminated unions are hoisted into `tagged.rs` (above). Recursive ones get `Box`ed variants.
- **typify name clashes:**
  - A nullable definition would become `struct X(Option<X>)`, so nullability moves to the use site.
  - `Foo.bar` vs `FooBar`: the inline type is hoisted or reused.
  - typify-gen fails with a readable message on a new clash.
- **Optional arrays** stay `Option<Vec<_>>`, because absent and `[]` can differ (`GetOlderSettledThreadsInput.projectIds`: absent means every project).
- **Validation keywords are stripped** (`maxLength`, `pattern`, integer bounds, `format`…), so a changed limit in T3 doesn't change Rust types. The server stays the authority.
- **Type mappings:**
  - `Schema.Number` becomes `JsNumber`, which accepts `"Infinity"`/`"-Infinity"`/`"NaN"`.
  - Brands are erased (`ThreadId` is a `String`), and `DateTimeUtc` is an ISO `String`.
  - `Unknown`/`Defect` become `serde_json::Value`.

## When regen fails

| Message | Fix |
|---|---|
| `contracts dependencies changed` | T3's contracts gained a dependency. Install it in regen.sh's work dir as well, with the version from T3's lockfile. |
| `could not read 'effect:'` | T3 moved its catalog. Update the `sed` in regen.sh. |
| `typify produced duplicate type names` | Add a collision rule in dump-schemas.ts ("typify names inline property types" section). |
| `typify emitted the serde-tagged enum …` | Extend the hoisting in dump-schemas.ts ("Hoist nested discriminated unions"). |
| `forward-compat: …` | A new lenient-field shape. Teach step 3b or `postprocess.rs::decoder_for`. |
| A fixture is `INVALID` | T3 changed the fixture's type. Update `tests/fixtures/t3-tests/` from T3's current tests. Never edit a fixture just to pass. |
| Round-trip failures | `cargo test -p t3_api --test roundtrip -- --nocapture` names the type and the first differing JSON path. |

An Effect version change means reviewing `dump-schemas.ts` (the internals above) and `src/envelope.rs` (Effect's RPC frames) by hand.

## Cost (T3 v0.1.1)

Measured on a 32-core Linux box, 2026-10-27.

| | |
|---|---|
| Surface | 198 RPC methods (31 streams), 66 HTTP endpoints, 1,230 definitions |
| Generated Rust | 109k lines, 3.9 MB: 320 string enums with fallback, 416 tagged unions, 45 lenient fields |
| Warm regen (`--no-test`) | ~1.5 s, of which dump-schemas takes 0.8 s |
| Cold regen | about 8 s plus the test build (typify-gen build 6 s, npm 0.6 s) |
| `cargo build -p t3_api`, clean (dependencies built, `CARGO_INCREMENTAL=0`) | 30 s, peak RSS 4.7 GB (a single rustc). Release: 39 s, 5.0 GB |
| `cargo test -p t3_api` build | 60 s incremental (peak 6.4 GB), 110 s without; `dispatch.rs` monomorphizes a round trip per type |
| Round trip | 1,910 Arbitrary values across 649 types, plus 7 T3 fixtures, all identical to T3's own re-encoding |
| Determinism | two runs give byte-identical output |
