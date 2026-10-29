# Third-party notices

mustr is built on Zed, whose own licenses are in `LICENSE-GPL`, `LICENSE-APACHE` and the notices
of each Zed crate. This file credits the third-party material mustr adds.

## Languages (`mustr-crates/mustr_languages`)

mustr compiles in the languages Zed gets from its extension registry, which mustr leaves out. For
each one, the language's `config.toml` and its queries (`highlights.scm`, `brackets.scm`,
`indents.scm`, `outline.scm`, `injections.scm` and `overrides.scm`, where the extension has them)
are copied unchanged from Zed's extension for it, at the revision Zed's extension index
(`zed-industries/extensions` at `809e0c5ac4`) pins, into `mustr-crates/mustr_languages/languages/`.
mustr's changes to them:

- `csharp/config.toml` also matches `.csx` files, and `xml/config.toml` also matches `.config`,
  `.nuspec`, `.plist`, `.resx`, `.ruleset`, `.runsettings`, `.svg`, `.xaml`, `.xsd` and `.xslt`.
- `ocaml-interface/highlights.scm` (a link to OCaml's in the extension) doesn't list `(shebang)`,
  a node the OCaml interface grammar doesn't have, so the query compiles.
- `elixir/injections.scm`, `nix/injections.scm` and `php/injections.scm` don't inject `comment`,
  a language mustr doesn't have: next to these languages' combined injections, its placeholder
  layers break the order Zed's syntax map keeps its layers in.
- SQL is highlighted with `tree-sitter-mssql`'s own `queries/highlights.scm`, with its capture names
  changed to the ones Zed's themes use (`@field` to `@property`, `@float` to `@number`,
  `@conditional` to `@keyword`, and no `@spell`), because the extension's queries are written for
  a grammar that doesn't parse T-SQL (see SQL below).

Grammars come from crates.io at the version the extension's grammar revision was released as, or
the closest release with the same node types. SQL's is the exception: the extension's grammar
(DerekStride/tree-sitter-sql) fails on most T-SQL (bracketed names, `@variables`, `SET`,
procedures, `IF … BEGIN … END`, `TRY … CATCH`), so mustr uses `tree-sitter-mssql`, which parses
them. Dart's, Dockerfile's and Vue's pinned revisions have
no crate that works with Zed's tree-sitter, so their generated parsers are vendored unchanged in
`mustr-crates/mustr_languages/grammars/`, each with its license. Svelte's comes from git at the
pinned revision.

| Language | Config and queries (license) | Grammar (license) |
|---|---|---|
| C# | `zed-extensions/csharp` `88597e19d1` (Apache-2.0) | `tree-sitter-c-sharp` 0.23.5, tree-sitter/tree-sitter-c-sharp (MIT) |
| C# Project File, MSBuild File, C# Solution File | `zed-extensions/csharp` `88597e19d1` (Apache-2.0) | `tree-sitter-xml` 0.7.0, tree-sitter-grammars/tree-sitter-xml (MIT) |
| Dart | `zed-extensions/dart` `fba74dfa66` (Apache-2.0) | UserNobody14/tree-sitter-dart `6922ad2914`, vendored (MIT) |
| Dockerfile | `zed-extensions/dockerfile` `b765430670` (Apache-2.0) | camdencheek/tree-sitter-dockerfile `868e44ce37`, vendored (MIT) |
| Elixir | `zed-extensions/elixir` `f02229c005` (Apache-2.0) | `tree-sitter-elixir` 0.3.4, elixir-lang/tree-sitter-elixir (Apache-2.0) |
| GraphQL | `11bit/zed-extension-graphql` `7337bb0ea4` (MIT) | `tree-sitter-graphql` 0.3.0, joowani/tree-sitter-graphql (MIT) |
| Haskell | `zed-extensions/haskell` `9a6687db33` (Apache-2.0) | `tree-sitter-haskell` 0.23.1, tree-sitter/tree-sitter-haskell (MIT) |
| HCL, Terraform, Terraform Vars | `zed-extensions/terraform` `99bcfa52db` (Apache-2.0) | `tree-sitter-hcl` 1.1.0, tree-sitter-grammars/tree-sitter-hcl (Apache-2.0) |
| HTML | Zed's `extensions/html` (Zed v1.25.3) (Apache-2.0) | `tree-sitter-html` 0.23.2, tree-sitter/tree-sitter-html (MIT) |
| Java | `zed-extensions/java` `2facaa7c40` (Apache-2.0) | `tree-sitter-java` 0.23.5, tree-sitter/tree-sitter-java (MIT) |
| Kotlin | `zed-extensions/zed-kotlin` `e94974ea06` (MIT) | `tree-sitter-kotlin-sg` 0.4.1, ast-grep/tree-sitter-kotlin, a release of fwcd/tree-sitter-kotlin (MIT) |
| Lua | `zed-extensions/lua` `ce3fde1f67` (Apache-2.0) | `tree-sitter-lua` 0.5.0, tree-sitter-grammars/tree-sitter-lua (MIT) |
| Make | `caius/zed-make` `f7c5052790` (MIT) | `tree-sitter-make` 1.1.1, tree-sitter-grammars/tree-sitter-make (MIT) |
| Nix | `hasit/zed-nix` `429479c09c` (MIT) | `tree-sitter-nix` 0.3.0, nix-community/tree-sitter-nix (MIT) |
| OCaml, OCaml Interface | `zed-extensions/ocaml` `e4d2901242` (Apache-2.0) | `tree-sitter-ocaml` 0.24.0, tree-sitter/tree-sitter-ocaml (MIT) |
| PHP | `zed-extensions/php` `6768398bea` (Apache-2.0) | `tree-sitter-php` 0.25.1, tree-sitter/tree-sitter-php (MIT) |
| Proto | Zed's `extensions/proto` (Zed v1.25.3) (Apache-2.0) | `tree-sitter-proto` 0.3.0, coder3101/tree-sitter-proto (MIT) |
| Ruby | `zed-extensions/ruby` `13ebcebfad` (Apache-2.0) | `tree-sitter-ruby` 0.23.1, tree-sitter/tree-sitter-ruby (MIT) |
| Scala | `scalameta/metals-zed` `5fe61bd8b9` (Apache-2.0) | `tree-sitter-scala` 0.24.0, tree-sitter/tree-sitter-scala (MIT) |
| SQL | `zed-extensions/sql` `fdd2a42ab9` (MIT) for the config, brackets, indents and injections; `tree-sitter-mssql` 0.2.3's highlights (MIT) | `tree-sitter-mssql` 0.2.3, alfpricot/tree-sitter-mssql, a T-SQL fork of DerekStride/tree-sitter-sql (MIT) |
| Svelte | `zed-extensions/svelte` `35f8d65c0e` (Apache-2.0) | tree-sitter-grammars/tree-sitter-svelte `3f06f70541`, from git (MIT) |
| Swift | `samuser107/zed-swift-extension` `302c7978f1` (Apache-2.0) | `tree-sitter-swift` 0.7.0, alex-pinkus/tree-sitter-swift (MIT) |
| TOML | `zed-extensions/toml` `8e18e50458` (Apache-2.0) | `tree-sitter-toml-ng` 0.7.0, tree-sitter-grammars/tree-sitter-toml (MIT) |
| Vue.js | `zed-extensions/vue` `7a57c87c8d` (Apache-2.0) | tree-sitter-grammars/tree-sitter-vue `7e48557b90`, vendored (MIT; it includes code from tree-sitter-html, MIT) |
| XML | `sweetppro/zed-xml` `d4750d4ec1` (MIT) | `tree-sitter-xml` 0.7.0, tree-sitter-grammars/tree-sitter-xml (MIT) |
| Zig | `zed-extensions/zig` `64fc8ac0fb` (Apache-2.0) | `tree-sitter-zig` 1.1.2, tree-sitter-grammars/tree-sitter-zig (MIT) |

Copyright holders of the MIT-licensed material above:

- Copyright (c) 2024 Evren Sen (zed-kotlin); Copyright (c) 2024 evrsen (Zed's SQL extension);
  Copyright (c) 2024 Caius Durling (zed-make); Copyright (c) 2025 SweetPPro (zed-xml);
  Copyright (c) 2024 Ivan Buryak (zed-extension-graphql); Copyright (c) 2024 Hasit Mistry (zed-nix).
- Copyright (c) 2014-2023 Max Brunsfeld, Damien Guard, Amaan Qureshi, and contributors
  (tree-sitter-c-sharp); Copyright (c) 2020-2023 UserNobody14 and others (tree-sitter-dart);
  Copyright (c) 2021 Camden Cheek (tree-sitter-dockerfile); Copyright (c) 2025 Joohwan Oh
  (tree-sitter-graphql); Copyright (c) 2019 fwcd (tree-sitter-kotlin); Copyright (c) 2019 Charles
  Strahan (tree-sitter-nix); Copyright (c) 2017 Josh Vera, GitHub (tree-sitter-php); Copyright (c)
  2018 Max Brunsfeld and GitHub (tree-sitter-scala); Copyright (c) Ika (tree-sitter-vue);
  Copyright (c) 2014 Max Brunsfeld (tree-sitter-html, which tree-sitter-vue includes code from);
  Copyright (c) 2021 Derek Stride (tree-sitter-sql, which tree-sitter-mssql started from; its
  T-SQL additions are by M. Reifschneider).
- The authors of tree-sitter-haskell, tree-sitter-html, tree-sitter-java, tree-sitter-lua,
  tree-sitter-make, tree-sitter-ocaml, tree-sitter-proto, tree-sitter-ruby, tree-sitter-swift, tree-sitter-toml, tree-sitter-xml and tree-sitter-zig, as listed in each
  crate.

The MIT license, under which each of them is distributed:

> Permission is hereby granted, free of charge, to any person obtaining a copy of this software and
> associated documentation files (the "Software"), to deal in the Software without restriction,
> including without limitation the rights to use, copy, modify, merge, publish, distribute,
> sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all copies or
> substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT
> NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
> NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
> DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT
> OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

The Apache-2.0 material (the extensions marked so, tree-sitter-elixir and tree-sitter-hcl) is
used under the Apache License, Version 2.0, whose text is in `LICENSE-APACHE` at the root of this
repository. Its copyright holders are the authors of each extension (Zed Industries, Inc. and the
contributors each extension lists) and of each grammar.
