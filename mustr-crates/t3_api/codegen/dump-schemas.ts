// Effect Schema -> JSON Schema (draft-07 bundle) + manifest for T3 Code's wire contracts.
//
// Usage: node dump-schemas.ts <contracts-src-dir> <out-dir> [--samples=N] [--fixtures=<dir>] [--only=<glob,...>]
//
// Must run from a directory whose node_modules/effect is the exact Effect version T3 pins
// (regen.sh arranges this). Uses Node's built-in TypeScript type stripping; T3 compiles
// with `erasableSyntaxOnly`, so no loader/transpiler is needed.
//
// Steps: 1 import every contracts module; 2 map encoded AST nodes -> TS export names;
// 3 collect roots (every WsRpcGroup RPC via `group.requests`, every HttpApi endpoint via
// `HttpApi.reflect`, plus EXTRA_ROOTS); 4 one shared `SchemaRepresentation` for all roots
// (on `Schema.toCodecJson`, i.e. exactly what RpcSerialization.layerJson sends) ->
// JSON Schema 2020-12 -> draft-07; 5 normalize for typify (see README.md); 6 write
// artifacts; 7 optional Arbitrary sample corpus + fixture validation.
//
// Outputs (in <out-dir>):
//   t3-api.raw.schema.json  draft-07 bundle as Effect emits it (defs renamed/merged only)
//   t3-api.schema.json      normalized for typify
//   manifest.json           RPC + HTTP method tables, tagged unions -> definition names
//   openapi.json            Effect's own OpenAPI 3.1 for the HTTP API (reference only)
//   stats.json              counts of every workaround applied (used by README.md)
//   samples/, fixtures-canonical/, fixtures-check.json, samples-failures.json (optional)

import { readdirSync, readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import * as Schema from "effect/Schema";
import * as SchemaAST from "effect/SchemaAST";
import * as SchemaRepresentation from "effect/SchemaRepresentation";
import * as JsonSchema from "effect/JsonSchema";
import * as RpcSchema from "effect/rpc/RpcSchema";
import * as HttpApi from "effect/http-api/HttpApi";
import * as OpenApi from "effect/http-api/OpenApi";
import * as Arbitrary from "effect/Arbitrary";
import * as Effect from "effect/Effect";

const positional = process.argv.slice(2).filter((a) => !a.startsWith("--"));
const flag = (name: string) => process.argv.find((a) => a.startsWith(`--${name}=`))?.split("=")[1];
const [srcDirArg, outDirArg] = positional;
if (!srcDirArg || !outDirArg) {
  console.error("usage: node dump-schemas.ts <contracts-src-dir> <out-dir> [--samples=N] [--fixtures=<dir>] [--only=<glob,...>]");
  process.exit(2);
}
// --samples=N: also write N Arbitrary-generated wire values per root type to <out>/samples/.
const SAMPLES = Number(flag("samples") ?? 0);
// --fixtures=<dir>: validate <dir>/<TypeName>[.label].json against T3's own JSON codec.
const FIXTURES = flag("fixtures");
// --only=<glob,...>: keep only matching RPC tags (e.g. `pullRequests.*`) and HTTP endpoints
// (`http:<group>.<endpoint>`, e.g. `http:auth.*`). Types are then limited to what those reach.
const ONLY = flag("only")?.split(",").filter(Boolean).map((g) => new RegExp("^" + g.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*") + "$"));
const selected = (id: string) => !ONLY || ONLY.some((r) => r.test(id));
const srcDir = resolve(srcDirArg);
const outDir = resolve(outDirArg);
mkdirSync(outDir, { recursive: true });

// Named types we always want as standalone Rust types even if no RPC/HTTP root reaches them.
const EXTRA_ROOTS = [
  "AuthTokenExchangeRequest",
  "AuthAccessTokenResult",
  "AuthWebSocketTicketResult",
  "ExecutionEnvironmentDescriptor",
];

const stats: Record<string, number> = {};
const bump = (k: string, n = 1) => (stats[k] = (stats[k] ?? 0) + n);

// ---------------------------------------------------------------------------
// 1. Load every contracts module and collect schemas / RPC groups / HTTP APIs.
// ---------------------------------------------------------------------------
const files = readdirSync(srcDir)
  .filter((f) => f.endsWith(".ts") && !f.endsWith(".test.ts") && !f.endsWith(".d.ts"))
  .sort((a, b) => (a === "index.ts" ? 1 : b === "index.ts" ? -1 : a.localeCompare(b)));

type Exported = { name: string; file: string; value: any; alias: boolean };
const exported: Array<Exported> = [];
for (const file of files) {
  let mod: Record<string, unknown>;
  try {
    mod = await import(pathToFileURL(join(srcDir, file)).href);
  } catch (e) {
    console.warn(`warn: failed to import ${file}: ${String(e).split("\n")[0]}`);
    bump("moduleImportFailures");
    continue;
  }
  // `export const A = B;` is an alias; prefer the original name for the Rust type.
  const aliases = new Set(
    [...readFileSync(join(srcDir, file), "utf8").matchAll(/^export const (\w+)\s*=\s*[A-Za-z_$][\w$]*\s*;/gm)].map(
      (m) => m[1],
    ),
  );
  for (const [name, value] of Object.entries(mod))
    exported.push({ name, file, value, alias: file === "index.ts" || aliases.has(name) });
}
bump("modules", files.length);
// Stable preference: non-alias exports first, then file order, then name.
exported.sort((a, b) => Number(a.alias) - Number(b.alias));

// RpcGroup values are callable class-likes; effect 4.0.5 exports no `isRpcGroup` guard.
const isRpcGroup = (u: any) =>
  u != null && u.requests instanceof Map && typeof u.toLayer === "function" && typeof u.middleware === "function";
const schemaExports = exported.filter((e) => Schema.isSchema(e.value));
const rpcGroups = uniqBy(exported.filter((e) => isRpcGroup(e.value)), (e) => e.value);
const httpApis = uniqBy(exported.filter((e) => HttpApi.isHttpApi(e.value)), (e) => e.value);

// ---------------------------------------------------------------------------
// 2. Name definitions after T3's TS export names.
//
// Effect only extracts `$defs` for schemas with an `identifier` annotation (classes and
// TaggedErrors get one automatically). T3 rarely annotates plain Structs/Unions, so by
// default everything else is inlined and typify would invent context-derived names.
// We pass a `referencePolicy` that maps the *encoded-side owner node* of each exported
// schema back to its export name. `toCodecJson` derivation is memoized per AST, so the
// nodes we compute here are the same objects the representation walk sees.
// (Mutating annotations in place does not work: T3 derives codecs at import time, so
// the memoized codec nodes already exist with the old annotations.)
// ---------------------------------------------------------------------------
const nameable = (ast: SchemaAST.AST): boolean => {
  switch (ast._tag) {
    case "Objects":
    case "Declaration":
      return true;
    case "Union": {
      // Skip `NullOr(X)` / `UndefinedOr(X)` wrappers: they should stay Option<X>.
      const real = ast.types.filter((t) => t._tag !== "Null" && t._tag !== "Undefined");
      return real.length > 1;
    }
    default:
      return false;
  }
};
const AST: any = SchemaAST; // getContextOwner / getLastEncoding / replaceEncoding are exported but @internal
const encodedOwner = (ast: SchemaAST.AST): SchemaAST.AST => AST.getContextOwner(AST.getLastEncoding(ast));
// Optional/contextual uses (`Schema.optional(X)`) see a shallow copy of X's encoded node,
// which shares X's member arrays. Key on those arrays so such sites resolve to X's name too.
const bodyKey = (ast: any): object =>
  ast._tag === "Objects"
    ? ast.propertySignatures.length > 0
      ? ast.propertySignatures
      : ast.indexSignatures.length > 0
        ? ast.indexSignatures
        : ast
    : ast._tag === "Union"
      ? ast.types
      : ast;

// Workaround for "opaque JSON" wrappers such as orchestrationV2IdTable.ts's
// `withRecordIdTable`: `Schema.declare(..., { toCodecJson: () => undefined }).pipe(decodeTo(codec))`.
// Its encoded side is an unconstrained declaration (JSON Schema `{}`), but the bytes on
// the wire are `codec`'s encoded form (plus an opt-in `ids` table we never request).
// We drop that last encoding link so the JSON Schema describes the real payload.
const unwrapped = new WeakMap<SchemaAST.AST, Schema.Top>();
const unwrapOpaqueJson = (schema: Schema.Top): Schema.Top => {
  const ast: any = schema.ast;
  const hit = unwrapped.get(ast);
  if (hit) return hit;
  let out = schema;
  const last = ast.encoding?.[ast.encoding.length - 1]?.to;
  const link = last?.annotations?.toCodecJson;
  if (
    last?._tag === "Declaration" &&
    last.typeParameters.length === 0 &&
    !last.encoding &&
    typeof link === "function" &&
    link([]) === undefined
  ) {
    const rest = ast.encoding.slice(0, -1);
    out = Schema.make(AST.replaceEncoding(ast, rest.length > 0 ? rest : undefined));
    bump("workaround.unwrapOpaqueJson");
  }
  unwrapped.set(ast, out);
  return out;
};

const nameByOwner = new Map<object, string>();
const ownerSource = new Map<object, SchemaAST.AST>();
const ambiguous = new Set<object>();
const seenAst = new Set<SchemaAST.AST>();
for (const { name, value } of schemaExports) {
  const ast: SchemaAST.AST = value.ast;
  if (seenAst.has(ast)) continue; // same AST exported twice: first (non-alias) name wins
  seenAst.add(ast);
  if (!nameable(ast)) continue;
  if (SchemaAST.resolveIdentifier(ast) !== undefined) bump("schemasAlreadyIdentified");
  const claim = (owner: object) => {
    const prev = ownerSource.get(owner);
    if (prev === undefined) {
      ownerSource.set(owner, ast);
      nameByOwner.set(owner, name);
    } else if (prev !== ast) {
      ambiguous.add(owner); // two different schemas share one encoded node: don't guess
    }
  };
  try {
    claim(bodyKey(encodedOwner(Schema.toCodecJson(unwrapOpaqueJson(value)).ast)));
  } catch {
    bump("schemasWithoutJsonCodec");
    continue;
  }
  // `Schema.catchDecoding` / `catchEncoding` (used by T3's ForwardCompatibleArray) rebuild
  // the encoded side from `toType(ast)` / `toEncoded(ast)`. Both are memoized, so register
  // those derived nodes under the same name as well.
  for (const derive of [SchemaAST.toType, SchemaAST.toEncoded]) {
    try {
      const derived = derive(ast);
      if (derived !== ast) claim(bodyKey(encodedOwner(Schema.toCodecJson(Schema.make(derived)).ast)));
    } catch {
      /* derived side has no JSON codec; ignore */
    }
  }
}
for (const o of ambiguous) nameByOwner.delete(o);
bump("exportedNameableSchemas", nameByOwner.size);
bump("ambiguousEncodedOwners", ambiguous.size);
let policyHits = 0;
// Export names win over Effect identifiers: identifiers can be inherited from a shared
// refinement (e.g. checkpointDiff.ts's `OrchestrationTurnDiffRange` filter is carried by
// three different structs via mapFields), which would yield `X`, `X_1`, `X_2`.
const referencePolicy = (input: { ast: SchemaAST.AST; identifier: string | undefined }) => {
  const n = nameByOwner.get(bodyKey(input.ast));
  if (n !== undefined) {
    policyHits++;
    return n;
  }
  return input.identifier;
};

// ---------------------------------------------------------------------------
// 3. Collect roots: every RPC (payload / success-or-chunk / error) and every HTTP endpoint.
// ---------------------------------------------------------------------------
type Root = { name: string; schema: Schema.Top };
const roots: Array<Root> = [];
const addRoot = (name: string, schema: Schema.Top) => {
  roots.push({ name, schema: unwrapOpaqueJson(schema) });
  return roots.length - 1;
};
const isNever = (s: Schema.Top) => s.ast._tag === "Never";

type RpcEntry = {
  group: string;
  tag: string;
  stream: boolean;
  payload: number;
  success: number; // for streams: element ("chunk") schema; exit success is Void
  error: number | null;
};
const rpcEntries: Array<RpcEntry> = [];
for (const { name: groupName, value: group } of rpcGroups) {
  for (const rpc of group.requests.values()) {
    if (!selected(rpc._tag)) continue;
    const base = pascal(rpc._tag);
    const streamSchemas = RpcSchema.isStreamSchema(rpc.successSchema) ? rpc.successSchema : undefined;
    const success: Schema.Top = streamSchemas ? (streamSchemas as any).success : rpc.successSchema;
    // Mirrors effect/rpc/Rpc.exitSchema: rpc error + stream error + middleware errors.
    const failures = new Set<Schema.Top>([rpc.errorSchema]);
    if (streamSchemas) failures.add((streamSchemas as any).error);
    for (const m of rpc.middlewares) failures.add(m.error);
    const errs = [...failures].filter((s) => s && !isNever(s));
    rpcEntries.push({
      group: groupName,
      tag: rpc._tag,
      stream: !!streamSchemas,
      payload: addRoot(`${base}Payload`, rpc.payloadSchema),
      success: addRoot(`${base}${streamSchemas ? "Chunk" : "Success"}`, success),
      error:
        errs.length === 0
          ? null
          : addRoot(`${base}RpcError`, errs.length === 1 ? errs[0]! : Schema.Union(errs)),
    });
  }
}

type HttpEntry = {
  api: string;
  group: string;
  endpoint: string;
  method: string;
  path: string;
  params: number | null;
  query: number | null;
  headers: number | null;
  payload: Array<{ contentType: string; root: number }>;
  success: Array<{ status: number; root: number }>;
  errors: Array<{ status: number; root: number }>;
};
const httpEntries: Array<HttpEntry> = [];
const unionOf = (schemas: ReadonlyArray<Schema.Top>) =>
  schemas.length === 1 ? schemas[0]! : Schema.Union(schemas as Array<Schema.Top>);
for (const { name: apiName, value: api } of httpApis) {
  HttpApi.reflect(api, {
    onGroup: () => {},
    onEndpoint: ({ group, endpoint, successes, errors }) => {
      const e: any = endpoint;
      if (!selected(`http:${group.identifier}.${e.identifier}`)) return;
      const base = `Http${pascal(group.identifier)}${pascal(e.identifier)}`;
      const payload: HttpEntry["payload"] = [];
      for (const [contentType, p] of e.payload as Map<string, { schemas: Array<Schema.Top> }>) {
        const suffix = payload.length === 0 ? "" : pascal(contentType);
        payload.push({ contentType, root: addRoot(`${base}Request${suffix}`, unionOf(p.schemas)) });
      }
      const successList = [...successes.entries()];
      httpEntries.push({
        api: apiName,
        group: group.identifier,
        endpoint: e.identifier,
        method: e.method,
        path: e.path,
        params: e.params ? addRoot(`${base}Params`, e.params) : null,
        query: e.query ? addRoot(`${base}Query`, e.query) : null,
        headers: e.headers ? addRoot(`${base}Headers`, e.headers) : null,
        payload,
        success: successList.map(([status, s]) => ({
          status,
          root: addRoot(successList.length === 1 ? `${base}Response` : `${base}Response${status}`, unionOf(s)),
        })),
        errors: [...errors.entries()].map(([status, s]) => ({
          status,
          root: addRoot(`${base}Error${status}`, unionOf(s)),
        })),
      });
    },
  });
}

const extraRootIdx: Record<string, number> = {};
for (const name of EXTRA_ROOTS) {
  const hit = schemaExports.find((e) => e.name === name);
  if (!hit) {
    console.warn(`warn: extra root ${name} not exported by contracts`);
    continue;
  }
  extraRootIdx[name] = addRoot(name, hit.value);
}
bump("roots", roots.length);
bump("rpcMethods", rpcEntries.length);
bump("rpcStreams", rpcEntries.filter((r) => r.stream).length);
bump("httpEndpoints", httpEntries.length);

// ---------------------------------------------------------------------------
// 4. One shared representation for all roots -> JSON Schema 2020-12 -> draft-07.
//    `toCodecJson` is exactly what RpcSerialization.layerJson uses on the wire.
// ---------------------------------------------------------------------------
const asts = roots.map((r) => Schema.toCodecJson(r.schema).ast) as [SchemaAST.AST, ...Array<SchemaAST.AST>];
const rep = SchemaRepresentation.toRepresentations(asts, { referencePolicy });
bump("referencePolicyHits", policyHits);
const multi = SchemaRepresentation.toJsonSchemaMultiDocument(rep, { generateDescriptions: false });

// Fold roots into the definitions pool. Roots that are a bare $ref alias the named def.
const defs: Record<string, any> = { ...multi.definitions };
const rootTarget: Array<{ kind: "ref"; def: string } | { kind: "void" } | { kind: "never" }> = [];
const DEF_PREFIX = "#/$defs/";
multi.schemas.forEach((s: any, i) => {
  const keys = Object.keys(s);
  if (keys.length === 1 && typeof s.$ref === "string" && s.$ref.startsWith(DEF_PREFIX)) {
    rootTarget[i] = { kind: "ref", def: decodePointer(s.$ref.slice(DEF_PREFIX.length)) };
  } else if (keys.length === 1 && s.type === "null") {
    rootTarget[i] = { kind: "void" };
  } else if (keys.length === 1 && s.not && Object.keys(s.not).length === 0) {
    rootTarget[i] = { kind: "never" };
  } else {
    let name = roots[i]!.name;
    name = uniqueName(name, (n) => n in defs);
    defs[name] = s;
    rootTarget[i] = { kind: "ref", def: name };
  }
});
const draft07 = JsonSchema.toDocumentDraft07({ dialect: "draft-2020-12", schema: {}, definitions: defs });

// Rename defs: drop Effect's `Encoded` suffix (we only ever model the encoded side) and
// make names Rust-friendly. Keep names unique and deterministic.
const renames = new Map<string, string>();
const used = new Set<string>();
for (const k of Object.keys(draft07.definitions).sort()) {
  let n = k.replace(/Encoded$/, "").replace(/[^A-Za-z0-9_]/g, "_");
  if (!/^[A-Za-z]/.test(n)) n = `T${n}`;
  n = uniqueName(n, (x) => used.has(x));
  used.add(n);
  renames.set(k, n);
}
const DEF07 = "#/definitions/";
const renameRefs = (node: any): any => {
  if (Array.isArray(node)) return node.map(renameRefs);
  if (node && typeof node === "object") {
    const out: any = {};
    for (const [k, v] of Object.entries(node)) {
      if (k === "$ref" && typeof v === "string" && v.startsWith(DEF07)) {
        const target = decodePointer(v.slice(DEF07.length));
        out[k] = DEF07 + encodePointer(renames.get(target) ?? target);
      } else out[k] = renameRefs(v);
    }
    return out;
  }
  return node;
};
const rawDefs: Record<string, any> = {};
for (const [k, v] of Object.entries(draft07.definitions).sort(([a], [b]) => a.localeCompare(b))) {
  rawDefs[renames.get(k)!] = renameRefs(v);
}
// Effect emits `X_1`, `X_2` when distinct-but-equivalent encoded nodes ask for the same
// name (contextual copies, catchDecoding re-derivations). Merge them back into `X` when
// they are structurally identical; repeat until stable since merges can cascade.
const merged = new Map<string, string>();
for (let changed = true; changed; ) {
  changed = false;
  const canon = (v: unknown) =>
    JSON.stringify(v, (k, x) =>
      k === "$ref" && typeof x === "string" && x.startsWith(DEF07)
        ? DEF07 + (merged.get(x.slice(DEF07.length)) ?? x.slice(DEF07.length))
        : x,
    );
  for (const k of Object.keys(rawDefs)) {
    const m = /^(.*)_(\d+)$/.exec(k);
    if (!m || merged.has(k) || !(m[1]! in rawDefs)) continue;
    const base = m[1]!;
    if (canon(rawDefs[k]).replaceAll(DEF07 + k + '"', DEF07 + base + '"') === canon(rawDefs[base])) {
      merged.set(k, base);
      changed = true;
    }
  }
}
for (const [dup, base] of merged) {
  delete rawDefs[dup];
  for (const [k, v] of renames) if (v === dup) renames.set(k, base);
}
const rewriteMerged = (node: any): any =>
  Array.isArray(node)
    ? node.map(rewriteMerged)
    : node && typeof node === "object"
      ? Object.fromEntries(
          Object.entries(node).map(([k, v]) =>
            k === "$ref" && typeof v === "string" && merged.has(v.slice(DEF07.length))
              ? [k, DEF07 + merged.get(v.slice(DEF07.length))]
              : [k, rewriteMerged(v)],
          ),
        )
      : node;
for (const k of Object.keys(rawDefs)) rawDefs[k] = rewriteMerged(rawDefs[k]);
bump("mergedDuplicateDefs", merged.size);
bump("unresolvedNumberedDefs", Object.keys(rawDefs).filter((k) => /_\d+$/.test(k)).length);

// (`nullableDefs` is filled by the normalization pass below; read lazily at manifest time.)
const typeOf = (i: number | null): any => {
  if (i === null) return { kind: "never" };
  const t = rootTarget[i]!;
  if (t.kind !== "ref") return t;
  const def = renames.get(t.def) ?? t.def;
  return nullableDefs.has(def) ? { kind: "ref", def, nullable: true } : { kind: "ref", def };
};
bump("definitions", Object.keys(rawDefs).length);

// ---------------------------------------------------------------------------
// 5. Normalize for typify (and for a lenient client): see README.md "workarounds".
// ---------------------------------------------------------------------------
const JS_NUMBER = "JsNumber";
const VALIDATION_KEYS = [
  "minLength", "maxLength", "pattern", "format", "contentEncoding", "contentMediaType",
  "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf",
  "uniqueItems", "minProperties", "maxProperties", "propertyNames",
];
const isNonFiniteStrings = (s: any) =>
  s && s.type === "string" && Array.isArray(s.enum) &&
  s.enum.length === 3 && ["Infinity", "-Infinity", "NaN"].every((x) => s.enum.includes(x));
const normalize = (node: any): any => {
  if (Array.isArray(node)) return node.map(normalize);
  if (!node || typeof node !== "object") return node;
  let s: any = { ...node };
  // `Schema.Struct({})` encodes as `{ not: { type: "null" } }` ("any non-null value");
  // on the wire it is `{}`. typify cannot convert `not`, so model it as an empty object.
  if (Object.keys(s).length === 1 && s.not && s.not.type === "null" && Object.keys(s.not).length === 1) {
    bump("norm.emptyStruct");
    return { type: "object", properties: {} };
  }
  // Effect's JSON codec encodes Schema.Number as number | "Infinity" | "-Infinity" | "NaN".
  const alts = s.anyOf ?? s.oneOf;
  if (Array.isArray(alts) && alts.length === 2 && Object.keys(s).length === 1) {
    const num = alts.find((a: any) => a.type === "number" && Object.keys(a).length === 1);
    const nf = alts.find(isNonFiniteStrings);
    if (num && nf) {
      bump("norm.jsNumber");
      return { $ref: DEF07 + JS_NUMBER };
    }
  }
  for (const k of VALIDATION_KEYS) {
    if (k in s) {
      // `const`-like single-value enums are kept; only constraints are dropped.
      delete s[k];
      bump(`norm.strip.${k}`);
    }
  }
  // Tuples with a rest element (`NonEmptyArray(X)` -> items: [X], additionalItems: X):
  // typify only supports fixed tuples, so widen to a plain array of the member union.
  if (Array.isArray(s.items) && s.additionalItems && typeof s.additionalItems === "object") {
    const all = [...s.items, s.additionalItems];
    const distinct = uniqBy(all, (x) => JSON.stringify(x));
    s.items = distinct.length === 1 ? distinct[0] : { anyOf: distinct };
    delete s.additionalItems;
    bump("norm.tupleWithRestToArray");
  }
  // Arrays: keep min/maxItems only for tuples (typify uses them to emit (A, B)).
  if (!Array.isArray(s.items)) {
    if ("minItems" in s) (delete s.minItems, bump("norm.strip.minItems"));
    if ("maxItems" in s) (delete s.maxItems, bump("norm.strip.maxItems"));
  }
  if (s.additionalProperties === true) {
    delete s.additionalProperties;
    bump("norm.dropAdditionalPropertiesTrue");
  }
  if (s.patternProperties) {
    // Records keyed by template literals: model as a plain string-keyed map.
    const vals = Object.values(s.patternProperties);
    const extra = s.additionalProperties && s.additionalProperties !== false ? [s.additionalProperties] : [];
    const all = [...vals, ...extra];
    s.additionalProperties = all.length === 1 ? all[0] : { anyOf: all };
    delete s.patternProperties;
    bump("norm.patternProperties");
  }
  // anyOf -> oneOf (typify renders anyOf as a struct of optional flattened members;
  // oneOf becomes a proper enum). serde tries variants in order, which matches Effect's
  // first-match union decoding closely enough for a client.
  if (Array.isArray(s.anyOf)) {
    s.oneOf = s.anyOf;
    delete s.anyOf;
    bump("norm.anyOfToOneOf");
  }
  if (Array.isArray(s.oneOf)) {
    // Flatten nested unions with no other keywords.
    const flat: Array<any> = [];
    for (const m of s.oneOf) {
      const mk = Object.keys(m);
      const inner = m.anyOf ?? m.oneOf;
      if (Array.isArray(inner) && mk.length === 1) flat.push(...inner);
      else flat.push(m);
    }
    s.oneOf = flat;
  }
  for (const [k, v] of Object.entries(s)) {
    if (k === "enum" || k === "const" || k === "default" || k === "examples") continue;
    if (v && typeof v === "object") s[k] = normalize(v);
  }
  // Single-member unions collapse.
  if (Array.isArray(s.oneOf) && s.oneOf.length === 1 && Object.keys(s).length === 1) s = s.oneOf[0];
  return s;
};
const normDefs: Record<string, any> = {};
for (const [k, v] of Object.entries(rawDefs)) normDefs[k] = normalize(v);
if (JS_NUMBER in normDefs) throw new Error(`contracts define ${JS_NUMBER}; pick another name`);
normDefs[JS_NUMBER] = {
  description: "Effect `Schema.Number` on the JSON wire: a number, or \"Infinity\" / \"-Infinity\" / \"NaN\".",
  type: "number",
};

// Union post-pass: splice in members of `$ref`'d pure unions (e.g. an RPC error union that
// contains `PullRequestRpcError`, itself a union) and drop duplicate members (the scope
// middleware's EnvironmentAuthorizationError often duplicates one already in the union).
const refName = (r: string) => decodePointer(r.slice(DEF07.length));
const isPureUnion = (s: any) =>
  s && Array.isArray(s.oneOf) && Object.keys(s).every((k) => k === "oneOf" || k === "description" || k === "title");
const flattenMembers = (members: Array<any>, depth = 0): Array<any> =>
  members.flatMap((m) => {
    const target = m && typeof m.$ref === "string" && Object.keys(m).length === 1 ? normDefs[refName(m.$ref)] : undefined;
    if (target && isPureUnion(target) && depth < 16) {
      bump("norm.flattenRefUnion");
      return flattenMembers(target.oneOf, depth + 1);
    }
    return [m];
  });
const unionPass = (node: any): any => {
  if (Array.isArray(node)) return node.map(unionPass);
  if (!node || typeof node !== "object") return node;
  let s: any = {};
  for (const [k, v] of Object.entries(node)) s[k] = k === "enum" || k === "const" ? v : unionPass(v);
  if (Array.isArray(s.oneOf)) {
    const before = s.oneOf.length;
    s.oneOf = uniqBy(flattenMembers(s.oneOf), (m) => JSON.stringify(m));
    if (s.oneOf.length < before) bump("norm.dedupeUnionMembers", before - s.oneOf.length);
    if (s.oneOf.length === 1 && Object.keys(s).length === 1) s = s.oneOf[0];
  }
  return s;
};
for (const k of Object.keys(normDefs)) normDefs[k] = unionPass(normDefs[k]);

// Nullable definitions (`X = oneOf [<object>, null]`): typify would emit
// `struct X(Option<X>)` plus an inner `X` and collide. Nullability belongs to the use
// site, so make `X` the non-null schema and rewrite every `$ref X` to `X | null`.
const isNullSchema = (m: any) => m && m.type === "null" && Object.keys(m).length === 1;
const nullableDefs = new Set<string>();
for (const [name, def] of Object.entries(normDefs)) {
  if (!isPureUnion(def) || def.oneOf.length !== 2) continue;
  const inner = def.oneOf.filter((m: any) => !isNullSchema(m));
  if (inner.length !== 1) continue;
  normDefs[name] = inner[0];
  nullableDefs.add(name);
}
if (nullableDefs.size > 0) {
  const rewriteNullable = (node: any): any => {
    if (Array.isArray(node)) return node.map(rewriteNullable);
    if (!node || typeof node !== "object") return node;
    if (typeof node.$ref === "string" && Object.keys(node).length === 1 && nullableDefs.has(refName(node.$ref)))
      return { oneOf: [node, { type: "null" }] };
    const out: any = {};
    for (const [k, v] of Object.entries(node)) out[k] = k === "enum" || k === "const" ? v : rewriteNullable(v);
    return out;
  };
  for (const k of Object.keys(normDefs)) normDefs[k] = unionPass(rewriteNullable(normDefs[k]));
}
bump("norm.nullableDefsInlined", nullableDefs.size);

// Discriminated unions at definition level -> "tagged union" entries in the manifest.
// typify can only emit serde internally-tagged enums when every member is an *inline*
// object (it does not follow `$ref`), and it has no unknown-variant fallback. For these
// defs the Rust generator emits a tag-dispatching enum (newtype variants + `Unrecognized`)
// instead. Inline object members are hoisted into their own definitions first.
type TaggedUnion = { tag: string; variants: Array<{ value: string; def: string }> };
const taggedUnions: Record<string, TaggedUnion> = {};
const resolveDef = (x: any, depth = 0): any =>
  x && typeof x.$ref === "string" && depth < 16 ? resolveDef(normDefs[refName(x.$ref)], depth + 1) : x;
const constString = (p: any): string | undefined => {
  const r = resolveDef(p);
  if (r && Array.isArray(r.enum) && r.enum.length === 1 && typeof r.enum[0] === "string") return r.enum[0];
  if (r && typeof r.const === "string") return r.const;
  return undefined;
};
// Members whose would-be tag is a plain `string` are catch-alls (T3's ForwardCompatibleUnion
// adds `{ [tag]: string }` so newer servers' variants decode); our `Unrecognized` variant
// covers them. Repeated tag values are allowed: candidates are tried in order.
const isPlainString = (p: any) => {
  const r = resolveDef(p);
  return !!r && r.type === "string" && !r.enum && r.const === undefined;
};
const discriminator = (members: Array<any>): { tag: string; catchAll: Set<number> } | undefined => {
  const objs = members.map(resolveDef);
  if (objs.some((o) => !o || o.type !== "object" || !o.properties)) return undefined;
  const consts = objs.map((o) => {
    const req = new Set(o.required ?? []);
    return Object.fromEntries(
      Object.entries(o.properties)
        .filter(([k]) => req.has(k))
        .map(([k, v]) => [k, constString(v)])
        .filter(([, v]) => v !== undefined),
    ) as Record<string, string>;
  });
  const candidates = [...new Set(consts.flatMap((c) => Object.keys(c)))].filter((k) =>
    objs.every((o, i) => k in consts[i]! || (isPlainString(o.properties[k]) && (o.required ?? []).includes(k))),
  );
  const score = (k: string) => {
    const vals = consts.filter((c) => k in c).map((c) => c[k]);
    const distinct = new Set(vals).size;
    if (distinct < 2) return -1;
    return (distinct === vals.length ? 100 : 0) + (k === "_tag" ? 3 : k === "type" ? 2 : k === "kind" ? 1 : 0);
  };
  const best = candidates.map((k) => [k, score(k)] as const).filter(([, sc]) => sc >= 0).sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))[0];
  if (!best) return undefined;
  const tag = best[0];
  return { tag, catchAll: new Set(consts.flatMap((c, i) => (tag in c ? [] : [i]))) };
};
// Hoist nested discriminated unions (property / array item / map value positions) into
// their own definitions so they get the same tag-dispatch enum instead of typify's
// `Variant0..N` untagged enums. Names follow typify's `<Parent><Prop>` convention.
const heckPascal0 = (s: string) =>
  s
    .split(/[^A-Za-z0-9]+/)
    .flatMap((p) => p.match(/[A-Z]+(?![a-z])|[A-Z]?[a-z0-9]+|[0-9]+/g) ?? [])
    .map((w) => w[0]!.toUpperCase() + w.slice(1).toLowerCase())
    .join("");
const hoistUnion = (sub: any, path: string): any => {
  if (!sub || typeof sub !== "object" || !Array.isArray(sub.oneOf) || !isPureUnion(sub)) return sub;
  const nonNull = sub.oneOf.filter((m: any) => !isNullSchema(m));
  const nullable = nonNull.length < sub.oneOf.length;
  if (nonNull.length < 2 || !discriminator(nonNull)) return sub;
  const name = uniqueName(heckPascal0(path), (n) => n in normDefs);
  normDefs[name] = { oneOf: nonNull };
  hoistQueue.push(name);
  bump("tagged.hoistedNestedUnions");
  const ref = { $ref: DEF07 + encodePointer(name) };
  return nullable ? { oneOf: [ref, { type: "null" }] } : ref;
};
const hoistWalk = (node: any, path: string) => {
  if (!node || typeof node !== "object") return;
  if (node.properties)
    for (const k of Object.keys(node.properties)) {
      node.properties[k] = hoistUnion(node.properties[k], `${path}_${k}`);
      hoistWalk(node.properties[k], `${path}_${k}`);
    }
  if (node.items && !Array.isArray(node.items)) {
    node.items = hoistUnion(node.items, `${path}_item`);
    hoistWalk(node.items, `${path}_item`);
  }
  if (node.additionalProperties && typeof node.additionalProperties === "object") {
    node.additionalProperties = hoistUnion(node.additionalProperties, `${path}_value`);
    hoistWalk(node.additionalProperties, `${path}_value`);
  }
  if (Array.isArray(node.oneOf)) node.oneOf.forEach((m: any) => hoistWalk(m, path));
};
const hoistQueue = Object.keys(normDefs);
while (hoistQueue.length) {
  const n = hoistQueue.shift()!;
  hoistWalk(normDefs[n], n);
}

for (const [name, def] of Object.entries(normDefs)) {
  if (!isPureUnion(def) || def.oneOf.length < 2) continue;
  const disc = discriminator(def.oneOf);
  if (disc === undefined) continue;
  const { tag, catchAll } = disc;
  if (catchAll.size > 0) bump("tagged.catchAllMembersFolded", catchAll.size);
  const variants = def.oneOf.flatMap((m: any, i: number) => {
    if (catchAll.has(i)) return [];
    const value = constString(resolveDef(m).properties[tag])!;
    if (typeof m.$ref === "string" && Object.keys(m).length === 1) return [{ value, def: refName(m.$ref) }];
    const hoisted = uniqueName(name + pascal(value), (n) => n in normDefs);
    normDefs[hoisted] = m;
    bump("tagged.hoistedInlineMembers");
    return [{ value, def: hoisted }];
  });
  def.oneOf = variants.map((v: { def: string }) => ({ $ref: DEF07 + encodePointer(v.def) }));
  taggedUnions[name] = { tag, variants };
}
bump("tagged.unions", Object.keys(taggedUnions).length);
// Recursive tagged unions need boxed variants (typify can't see through our replacement).
const refsOf = (node: any, out = new Set<string>()): Set<string> => {
  if (Array.isArray(node)) node.forEach((n) => refsOf(n, out));
  else if (node && typeof node === "object")
    for (const [k, v] of Object.entries(node)) {
      if (k === "$ref" && typeof v === "string") out.add(refName(v));
      else refsOf(v, out);
    }
  return out;
};
const reaches = (from: string, target: string) => {
  const seen = new Set<string>();
  const stack = [from];
  while (stack.length) {
    const n = stack.pop()!;
    if (n === target) return true;
    if (seen.has(n)) continue;
    seen.add(n);
    for (const r of refsOf(normDefs[n])) stack.push(r);
  }
  return false;
};
for (const [name, u] of Object.entries(taggedUnions)) {
  (u as any).recursive = u.variants.some((v) => reaches(v.def, name));
  if ((u as any).recursive) bump("tagged.recursive");
}

// typify names inline property types `<Parent><Prop>` (and array items `<Parent><Prop>Item`)
// without checking existing definitions, so T3's common `FooBar` + `Foo.bar` naming
// collides (e.g. ScheduledTaskWebhookDelivery.outcome vs ScheduledTaskWebhookDeliveryOutcome).
// Where that would happen: reuse the definition if identical, else hoist under a free name.
const heckWords = (s: string) =>
  s
    .split(/[^A-Za-z0-9]+/)
    .flatMap((p) => p.match(/[A-Z]+(?![a-z])|[A-Z]?[a-z0-9]+|[0-9]+/g) ?? [])
    .filter(Boolean);
const heckPascal = (s: string) =>
  heckWords(s)
    .map((w) => w[0]!.toUpperCase() + w.slice(1).toLowerCase())
    .join("");
const typifyNames = new Set(Object.keys(normDefs).map(heckPascal));
const producesNamedType = (x: any) =>
  x && typeof x === "object" && typeof x.$ref !== "string" &&
  ((x.type === "object" && x.properties) || Array.isArray(x.enum) || (Array.isArray(x.oneOf) && !(x.oneOf.length === 2 && x.oneOf.some(isNullSchema) && x.oneOf.every((m: any) => isNullSchema(m) || typeof m.$ref === "string" || (typeof m.type === "string" && !m.properties && !m.enum)))));
const fixCollisions = (node: any, prefix: string) => {
  if (!node || typeof node !== "object" || !node.properties) return;
  for (const [prop, sub] of Object.entries<any>(node.properties)) {
    const derived = heckPascal(`${prefix}_${prop}`);
    if (producesNamedType(sub) && typifyNames.has(derived)) {
      const same = Object.entries(normDefs).find(([k, v]) => heckPascal(k) === derived && JSON.stringify(v) === JSON.stringify(sub));
      if (same) {
        node.properties[prop] = { $ref: DEF07 + encodePointer(same[0]) };
        bump("norm.collision.reusedDef");
      } else {
        const name = uniqueName(derived, (n) => n in normDefs || typifyNames.has(heckPascal(n)));
        normDefs[name] = sub;
        typifyNames.add(heckPascal(name));
        node.properties[prop] = { $ref: DEF07 + encodePointer(name) };
        bump("norm.collision.hoisted");
        fixCollisions(sub, name);
      }
      continue;
    }
    fixCollisions(sub, `${prefix}_${prop}`);
    if (sub && sub.type === "array" && sub.items && !Array.isArray(sub.items)) fixCollisions(sub.items, `${prefix}_${prop}_item`);
  }
};
for (const k of Object.keys(normDefs)) fixCollisions(normDefs[k], k);

// Optional arrays/maps: typify models a non-required `array` as `Vec<T>` with
// `skip_serializing_if = "Vec::is_empty"`, which conflates "absent" with `[]` (T3 has
// fields where they differ, e.g. GetOlderSettledThreadsInput.projectIds: absent = every
// project). Make them `T | null` so typify emits `Option<Vec<T>>` instead.
const isCollection = (x: any): boolean => {
  const r = resolveDef(x);
  return !!r && (r.type === "array" || (r.type === "object" && !r.properties && r.additionalProperties !== undefined));
};
const optionalCollections = (node: any) => {
  if (Array.isArray(node)) return node.forEach(optionalCollections);
  if (!node || typeof node !== "object") return;
  if (node.type === "object" && node.properties) {
    const req = new Set(node.required ?? []);
    for (const [k, v] of Object.entries<any>(node.properties)) {
      if (!req.has(k) && isCollection(v)) {
        node.properties[k] = { oneOf: [v, { type: "null" }] };
        bump("norm.optionalCollectionAsOption");
      }
    }
  }
  for (const [k, v] of Object.entries(node)) if (k !== "enum" && k !== "const") optionalCollections(v);
};
for (const k of Object.keys(normDefs)) optionalCollections(normDefs[k]);

// ---------------------------------------------------------------------------
// 6. Write artifacts.
// ---------------------------------------------------------------------------
const bundle = (definitions: Record<string, any>) => ({
  $schema: "http://json-schema.org/draft-07/schema#",
  title: "T3 Code wire contracts (encoded/JSON side)",
  definitions,
});
const write = (f: string, v: unknown) => writeFileSync(join(outDir, f), JSON.stringify(v, null, 2) + "\n");
write("t3-api.raw.schema.json", bundle(rawDefs));
write("t3-api.schema.json", bundle(normDefs));
write("manifest.json", {
  rpc: rpcEntries.map((r) => ({
    group: r.group,
    tag: r.tag,
    stream: r.stream,
    payload: typeOf(r.payload),
    success: typeOf(r.success),
    error: typeOf(r.error),
  })),
  http: httpEntries.map((h) => ({
    api: h.api,
    group: h.group,
    endpoint: h.endpoint,
    method: h.method,
    path: h.path,
    params: h.params === null ? null : typeOf(h.params),
    query: h.query === null ? null : typeOf(h.query),
    headers: h.headers === null ? null : typeOf(h.headers),
    payload: h.payload.map((p) => ({ contentType: p.contentType, type: typeOf(p.root) })),
    success: h.success.map((p) => ({ status: p.status, type: typeOf(p.root) })),
    errors: h.errors.map((p) => ({ status: p.status, type: typeOf(p.root) })),
  })),
  named: Object.fromEntries(Object.entries(extraRootIdx).map(([n, i]) => [n, typeOf(i)])),
  taggedUnions,
});
for (const { value: api } of httpApis) write("openapi.json", OpenApi.fromApi(api));

// ---------------------------------------------------------------------------
// 7. Optional: sample wire values (round-trip corpus for the Rust crate) and fixture check.
// ---------------------------------------------------------------------------
const schemaByDef = new Map<string, Schema.Top>();
roots.forEach((r, i) => {
  const t = typeOf(i);
  if (t.kind === "ref" && !schemaByDef.has((t as any).def)) schemaByDef.set((t as any).def, r.schema);
});
for (const n of EXTRA_ROOTS) {
  const hit = schemaExports.find((e) => e.name === n);
  if (hit && !schemaByDef.has(n)) schemaByDef.set(n, unwrapOpaqueJson(hit.value));
}
if (SAMPLES > 0) {
  const dir = join(outDir, "samples");
  mkdirSync(dir, { recursive: true });
  let ok = 0, viaEncoded = 0, failed = 0;
  const failures: Record<string, string> = {};
  for (const [def, schema] of [...schemaByDef].sort(([a], [b]) => a.localeCompare(b))) {
    const codec = Schema.toCodecJson(schema);
    const sample = (arb: any) => Effect.runSync(Arbitrary.sampleEffect(arb, { count: SAMPLES, seed: 42, size: 6 }));
    let values: ReadonlyArray<unknown> | undefined;
    try {
      // Realistic values: generate the decoded Type, then encode with T3's own JSON codec.
      const encode = Schema.encodeSync(codec as any);
      values = sample(Arbitrary.schema(schema as any)).map((v) => encode(v));
      ok++;
    } catch (e1) {
      try {
        // Fallback: generate the encoded (JSON) side directly (no transformations run).
        values = sample(Arbitrary.schema(Schema.make(SchemaAST.toEncoded(codec.ast)) as any));
        viaEncoded++;
      } catch (e2) {
        failed++;
        failures[def] = String(e2).split("\n")[0]!.slice(0, 200);
        continue;
      }
    }
    writeFileSync(join(dir, `${def}.json`), JSON.stringify(values.map(wellFormed)) + "\n");
  }
  bump("samples.typeSide", ok);
  bump("samples.encodedSide", viaEncoded);
  bump("samples.failed", failed);
  write("samples-failures.json", failures);
}
if (FIXTURES) {
  const results: Record<string, string> = {};
  for (const f of readdirSync(FIXTURES).filter((f) => f.endsWith(".json")).sort()) {
    const def = f.split(".")[0]!;
    const schema = schemaByDef.get(def) ?? schemaExports.find((e) => e.name === def)?.value;
    if (!schema) {
      results[f] = "no such type";
      continue;
    }
    try {
      // Decode with T3's codec, then re-encode: the canonical wire form T3 itself would send.
      const codec = Schema.toCodecJson(schema) as any;
      const decoded = Schema.decodeUnknownSync(codec)(JSON.parse(readFileSync(join(FIXTURES, f), "utf8")));
      mkdirSync(join(outDir, "fixtures-canonical"), { recursive: true });
      writeFileSync(join(outDir, "fixtures-canonical", f), JSON.stringify(Schema.encodeSync(codec)(decoded), null, 2) + "\n");
      results[f] = "ok";
    } catch (e) {
      results[f] = "INVALID: " + String(e).split("\n").slice(0, 3).join(" ").slice(0, 300);
    }
  }
  write("fixtures-check.json", results);
  const bad = Object.entries(results).filter(([, v]) => v !== "ok");
  console.log(`fixtures: ${Object.keys(results).length - bad.length}/${Object.keys(results).length} decode with T3's codec`);
  for (const [f, v] of bad) console.log(`  ${f}: ${v}`);
}
console.log(
  `dump-schemas: ${stats.rpcMethods} rpc methods (${stats.rpcStreams} streams), ` +
    `${stats.httpEndpoints} http endpoints, ${stats.definitions} definitions -> ${outDir}`,
);

write("stats.json", stats);

// ---------------------------------------------------------------------------
function pascal(s: string): string {
  return s
    .split(/[^A-Za-z0-9]+/)
    .filter(Boolean)
    .map((p) => p[0]!.toUpperCase() + p.slice(1))
    .join("");
}
// Numeric suffixes, not `_`: typify Pascal-cases names, which would fold `X_` into `X`.
function uniqueName(base: string, taken: (n: string) => boolean): string {
  if (!taken(base)) return base;
  for (let i = 2; ; i++) if (!taken(`${base}${i}`)) return `${base}${i}`;
}
// Arbitrary strings may contain lone UTF-16 surrogates; JSON.stringify escapes them but
// serde_json (UTF-8) rejects them. Real T3 data is not expected to contain them.
function wellFormed(v: unknown): unknown {
  if (typeof v === "string") return (v as any).toWellFormed();
  if (Array.isArray(v)) return v.map(wellFormed);
  if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [wellFormed(k) as string, wellFormed(x)]));
  return v;
}
function uniqBy<T>(xs: Array<T>, key: (x: T) => unknown): Array<T> {
  const seen = new Set();
  return xs.filter((x) => (seen.has(key(x)) ? false : (seen.add(key(x)), true)));
}
function decodePointer(s: string) {
  return s.replace(/~1/g, "/").replace(/~0/g, "~");
}
function encodePointer(s: string) {
  return s.replace(/~/g, "~0").replace(/\//g, "~1");
}
