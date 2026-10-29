//! Compiles the grammars mustr vendors (`grammars/`): the ones whose pinned revisions have no
//! crate that works with Zed's tree-sitter. Each directory holds the grammar's generated parser,
//! its scanner, and its license, copied unchanged from the revision `THIRD_PARTY_NOTICES.md` names.

use std::path::Path;

fn main() {
    for name in ["dart", "dockerfile", "vue"] {
        let dir = Path::new("grammars").join(name);
        let mut build = cc::Build::new();
        build.std("c11").include(&dir).warnings(false);
        for file in ["parser.c", "scanner.c"] {
            build.file(dir.join(file));
        }
        println!("cargo:rerun-if-changed={}", dir.display());
        build.compile(&format!("tree-sitter-{name}"));
    }
}
