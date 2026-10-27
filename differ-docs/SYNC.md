# SYNC.md: keeping Differ current with Zed and T3 Code

This runbook is for an AI agent. The scripts are `script/differ-sync-zed` and `script/differ-sync-t3`.

Both scripts are idempotent: re-running one picks up where the last run stopped. Each ends with a `==== summary ====` block of `key: value` lines; read that first. They never push, never touch `main`, and never edit Zed-owned files outside a merge.

| Exit | Zed script | T3 script | What you do |
|---|---|---|---|
| 0 | Merged, already up to date, or dry run | Synced, already pinned, or dry run | Continue at "Finish" |
| 1 | Preflight error. Nothing changed | Same | Fix the environment (the message says how), then re-run |
| 2 | Stop and ask the human | Stop and ask the human | Report the `reason:` line verbatim |
| 3 | Build or tests failed. The merge commit is on the sync branch | Same, for the sync commit | Fix it in `differ-crates/`, commit, re-run |
| 4 | Invariant violated: Zed-owned files differ from the tag | Codegen failed. Nothing committed | Zed: ask the human. T3: fix codegen, then re-run |

## 0. Ground rules

- **Zed-owned** means every path except the Differ-owned paths and the touch points below. Never edit a Zed-owned path. A fix that needs one is a Zed patch, so ask the human.
- **Differ-owned paths:** `differ-crates/`, `differ-docs/`, `assets/themes/differ/`, `script/differ-*`, `.github/workflows/differ-*`, `THIRD_PARTY_NOTICES.md`. Upstream never ships these. The Zed script checks.
- **Touch points**, the only Zed files Differ changes:
  - `README.md`, `CLAUDE.md`, `AGENTS.md`, and `.github/FUNDING.yml`: Differ's version wins. `CLAUDE.md` and `AGENTS.md` are symlinks to `differ-docs/AGENTS.md`. Differ deleted FUNDING.yml, and it stays deleted.
  - Root `Cargo.toml`: Zed's version plus Differ's `"differ-crates/*",` members entry. On branch `m0/t3-client` this is a 6-line block, the entry plus comments, at the end of `members`. The script re-applies Differ's diff. If the patch context has drifted, it inserts the block right after `members = [`.
  - `Cargo.lock`: Zed's lockfile, with Differ's crates added by cargo.
- Differ-only dependencies go in `differ-crates/*/Cargo.toml` with explicit versions. Never add them to the root `[workspace.dependencies]`, because that would create a second Cargo.toml touch point.
- Never force-push `main`. Never push to `upstream`; its push URL is `no-pushing-to-zed`.
- Zed tags are fetched to `refs/upstream-tags/<tag>`, never `refs/tags`, so `git push --tags` can't push Zed's tags to `origin`.
- Script state and logs go in `.git/differ-sync/`: PR bodies, the T3 sparse checkout, contract diffs, and the codegen log.

**Pins**, all in Differ-owned files:

| File | Example | Written by |
|---|---|---|
| `differ-docs/ZED_VERSION` | `v1.25.3` | `differ-sync-zed` |
| `differ-crates/t3_client/T3_VERSION` | `t3_server_version=0.1.3`, `orchestration_protocol=2`, `contracts_commit=<sha>`: key=value, the format `m0/t3-client` chose | `differ-sync-t3` |
| `differ-crates/t3_client/T3_RUNTIME.sha256` | `sha256sum` lines for `t3-<v>-<os>-<arch>.{tar.gz,zip}` | `differ-sync-t3` |

**Tools.** git 2.38 or later, curl, jq, `gh` logged in as `msetaro` (for releases and PRs), and cargo via rustup (`rust-toolchain.toml` pins 1.98.1).
- On 2026-10-27 this machine had no Rust toolchain, so the scripts' build steps fail with exit 3. Use `--no-build` until cargo is installed.
- **Needs the human:**
  - Install rustup in user space: `curl https://sh.rustup.rs | sh`.
  - Once, run `script/linux` for Zed's system build dependencies. It uses `sudo apt-get`/`dnf`.

## 1. When to run

| Upstream | Trigger | Check |
|---|---|---|
| Zed | Each new stable release, about weekly, plus `vX.Y.Z` patch releases (hotfixes). Ignore `-pre` (preview) tags and the moving `nightly` tag. | `gh api repos/zed-industries/zed/releases/latest --jq .tag_name` differs from `ZED_VERSION` |
| T3 Code | Each new stable release, every few days. Never pin `-preview.` tags; T3 labels them maintainer test builds. Pin `-nightly.` tags only on throwaway experiment branches. | `gh api repos/pingdotgg/t3code/releases/latest --jq .tag_name` differs from `t3_server_version` in `T3_VERSION` |

- Run each sync on its own branch and PR (`sync/zed-<tag>`, `sync/t3-<tag>`). If both are due, merge one PR before starting the other.
- Tag schemes, checked 2026-10-27:
  - Zed: stable `v1.25.3`, preview `v1.26.2-pre`.
  - T3: stable `v0.1.3`, nightly `v0.1.4-nightly.20261027.3783`, preview `v0.1.3-preview.20261026.3702`.
- A scheme change makes the scripts stop with exit 1 or 2.

## 2. First Zed sync (done)

On 2026-10-27, `main` was recreated on Zed's stable tag v1.25.3 (option B, approved by the maintainer) instead of Zed's untagged `main`. Every sync since then is a normal run of `differ-sync-zed`. If `main` ever has to move to a Zed version that isn't a descendant of the pinned tag, use `--rebaseline`: it makes a merge commit, so it never needs a force-push.

## 3. `differ-sync-zed`

```sh
script/differ-sync-zed --dry-run     # tag + predicted conflicts, no changes
script/differ-sync-zed               # merge into sync/zed-<tag>, build, test
```

**What it does:**
1. Picks the latest published stable release. It cross-checks `gh releases/latest` against a strict `^v\d+\.\d+\.\d+$` match on the tags, so a tag whose release isn't published yet is skipped.
2. Checks that `main` matches the previous tag on every Zed-owned path. If it doesn't, it stops with exit 4.
3. Runs `git merge --no-ff --no-commit <tag>`.
4. Writes the known-correct tree: Zed-owned paths and `Cargo.toml`/`Cargo.lock` are set to the tag, then the Differ-owned paths, `README.md`, and `FUNDING.yml` are restored. Differ's `Cargo.toml` diff is re-applied, and `cargo metadata` adds Differ's crates to `Cargo.lock`.
5. Verifies the result and commits with the tag as the second parent.
6. Builds `differ` and tests Differ's crates. A smoke test runs if `script/differ-smoke-test` exists.

**Expected conflicts, all auto-resolved.** Plan on more than the README and Cargo.toml touch points:
- **`crates/zed/Cargo.toml` `version`.** Every release branch bumps it, and the merge resolves to Zed's value.
- **`Cargo.lock`, `crates/gpui_wgpu/Cargo.toml` (wgpu rev), and source files Zed cherry-picked onto the release branch.** For v1.24.4 → v1.25.3 these were edit_prediction, google_ai, and node_runtime, 6 Zed-owned files in all, resolved to Zed's version.
- **`crates/zed/RELEASE_CHANNEL`** becomes `stable` (it is `dev` on `main`). That's expected. Differ's builds must set `ZED_RELEASE_CHANNEL` themselves (see REBRAND.md).
- The summary lists every natural conflict under `touch point` or `zed-owned`. A `DIFFER-OWNED` conflict means stop: the script exits 2.

**Copied code.** `crates/zed` has no `[lib]`, so Differ copies parts of it, such as window options and the open listener (REBRAND.md §4). Each copy carries a header of the form `// Copied from crates/<path>@<tag> <item>`.
- The summary's `copied_code` line lists the upstream files that changed, and the full diff is in `.git/differ-sync/zed-<tag>-copied.diff`.
- Port the hunks that matter, such as bug fixes and API changes, and bump the `@<tag>` in the header.
- Skip Zed-only behavior, such as collab, hosted AI, and onboarding.

**If the build fails (exit 3),** Zed's API changed under Differ's crates:
- Read the `changed zed crate` lines and `git log refs/upstream-tags/<old>..refs/upstream-tags/<new> -- crates/<name>`.
- Fix only in `differ-crates/`. Commit on the sync branch as `Adapt to Zed <tag>: …`, then re-run the script. It sees the existing branch, re-checks the invariant, and rebuilds.
- Stop and ask if a fix seems to need a Zed patch, or if more than about 300 lines of Differ code change.

**If Cargo.toml's diff won't re-apply (exit 2):**
- Upstream reshaped the `members` list. Look at `.git/differ-sync-cargo-toml.rejected.patch`.
- If the only change is still the members line, re-add `"differ-crates/*",` after `members = [` with the manual steps in section 6.
- Anything else, ask the human.

**Before opening the PR, check:**
- `invariant: OK`.
- `git diff refs/upstream-tags/<tag> HEAD --stat` lists only the touch points and Differ-owned paths.
- `git diff refs/upstream-tags/<tag> HEAD -- Cargo.lock` only adds Differ's crates and their dependencies. Differ-only dependencies can move to newer semver-compatible versions; note that in the PR.
- `new_upstream_workflows`. After the PR merges, run `gh workflow disable <file> -R msetaro/differ` for each one; this needs the human's gh token with admin rights. Never delete them, because deleting causes merge conflicts.

## 4. `differ-sync-t3`

```sh
script/differ-sync-t3 --latest --dry-run   # tag, protocol, contracts diff, checksums; no changes
script/differ-sync-t3 v0.1.3               # sync/t3-v0.1.3: codegen, pins, commit, build, tests
```

**What it does:**
1. Checks that the tag and its GitHub release exist and that the release is not a draft.
2. Sparse-clones that tag's `packages/contracts` to `.git/differ-sync/t3-src-<tag>/`.
3. Reads `ORCHESTRATION_PROTOCOL_VERSION`, compares the contracts with the previous pin (excluding `*.test.ts`), and checks that T3's LICENSE is still MIT and unchanged.
4. Downloads `SHA256SUMS`, requires all 5 runtime archives, and cross-checks them against GitHub's asset digests.
5. Runs codegen (`regen.sh <t3-checkout> <out-dir>`, a placeholder for m0/t3-codegen; skipped while the file doesn't exist), writes the pins, and commits.
6. Downloads the host's runtime archive to `~/.cache/differ-dev/t3-runtime/<v>/`, verifies its SHA-256, and extracts it.
7. Runs `cargo build`, `cargo test -p t3_client`, and the contract tests (`DIFFER_CONTRACT_TEST_CMD`, a placeholder) with `DIFFER_T3_RUNTIME_BIN` set.
8. Removes `T3CODE_*`/`T3_*` from the environment of anything it spawns.

**Protocol version.**
- `packages/contracts/src/environment.ts` exports `ORCHESTRATION_PROTOCOL_VERSION`. It is 2 from v0.1.0 through v0.1.3 and the 2026-10-27 nightlies, and was 1 at v0.0.45.
- The server accepts only an exact match: `x-t3-orchestration-protocol: 2` on HTTP and `?orchestrationProtocol=2` on WebSocket. Anything else gets HTTP 426.
- `orchestration_protocol` in `T3_VERSION` must equal T3's value, and so must Differ's constant `pub const ORCHESTRATION_PROTOCOL_VERSION: u32 = 2;` in `t3_client/src/auth.rs`. The script checks both and prints `protocol_const`.
- Add a unit test to `t3_client` that asserts the constant equals the pin (`include_str!("../T3_VERSION")`). That way `cargo test` catches drift too.
- If the value changes, the script stops with exit 2. That's a human decision, because a Differ build on protocol N+1 can't talk to a user's installed T3 on protocol N. That breaks the "connect to existing T3" path until they update.
- When the human approves, re-run with `--accept-protocol-change`.
  - The script commits the pin and exits 3 with `protocol_const: ACTION`. Update the constant, commit, and re-run.
  - Then update Differ's supported-server range and its connect-time check (`check_protocol_compatibility` in `auth.rs`), so that an old T3 gets the "update T3 or use Differ's runtime" prompt.

**When the generated types change.** Read `generated_types`, `.git/differ-sync/t3-generated-<tag>.diffstat`, and the contracts diff `.git/differ-sync/t3-contracts-<tag>.diff`. From v0.1.1 to v0.1.3, 57 non-test files changed, with +1692/−1488 lines, so expect real churn.
- **Additive changes** (new optional fields, new RPCs, new union members): these usually compile.
  - Make sure decoding of server-to-client unions tolerates unknown variants (`#[serde(other)]` or a catch-all), so a newer T3 doesn't break an older Differ.
  - Add a match arm only where Differ shows the new variant.
- **Renamed or removed fields or RPCs:** the compiler lists every call site. Fix them in Differ's wrappers and UI crates.
  - Never hand-edit `src/generated/`. Fix the codegen, or wrap the type outside `generated/`.
  - If an RPC Differ uses (PLAN.md §4) is removed with no replacement, stop and ask.
- **Same shape, new meaning:** the compiler can't catch this.
  - Read the contracts diff for every RPC Differ uses, plus `auth.ts`, `environment.ts`, and `environmentHttp.ts`.
  - The contract tests (record and replay) should fail. Re-record fixtures only for intended wire changes, and say so in the PR. Never edit a fixture just to make a test pass.
- **New Effect version** (`effect_version`): codegen must use exactly that version. If codegen fails (exit 4), fix the codegen, not the contracts.

**Checksums.**
- `T3_RUNTIME.sha256` comes from the release's `SHA256SUMS` and must match GitHub's digests. T3 publishes no signatures, so this is TLS plus GitHub plus Differ's pin.
- On a mismatch the script exits 2. Never re-pin over a mismatch; tell the human, since it could be tampering.
- T3 ships no darwin-x64 (Intel Mac) runtime. If any of the 5 expected platforms is missing, the script exits 2.

## 5. Stop and ask the human when

- Any exit code 2, or exit 4 from the Zed script.
- The first Zed sync (section 2), or anything that needs a force-push.
- A fix seems to need an edit to a Zed-owned file, a new touch point, or a root `[workspace.dependencies]` entry.
- `ORCHESTRATION_PROTOCOL_VERSION` changes, or a release drops support for T3 versions users have installed.
- An RPC Differ uses is removed, or its meaning changed and the fix isn't obvious.
- A checksum mismatch, a missing platform archive, a change to T3's LICENSE, or a new license in Zed's tree (`legal/`, `LICENSE-*`).
- A change to the tag scheme, or Zed or T3 moves the files the scripts read (`crates/zed/Cargo.toml` version, `packages/contracts/src/environment.ts`, `SHA256SUMS`).
- The build is still broken after one honest fix attempt of about 300 lines or 1 hour.
- Enabling, disabling, or editing GitHub workflows, repo settings, or secrets.

## 6. Finish

1. Push and open the PR using the `next:` line from the summary (`gh pr create … --body-file .git/differ-sync/<…>.md`).
2. Wait for Differ's CI (`.github/workflows/differ-*.yml`).
3. Merge only if the human has allowed agents to merge green sync PRs. Otherwise hand the PR to them.
4. After a Zed sync merges, disable the new workflows listed in `new_upstream_workflows`.

**Manual Zed merge,** if the script can't run. These are the same steps the script takes:
```sh
T=v1.26.3; OLD=$(cat differ-docs/ZED_VERSION)
git fetch --no-tags upstream +refs/tags/$T:refs/upstream-tags/$T +refs/tags/$OLD:refs/upstream-tags/$OLD
git switch -c sync/zed-$T main
git merge --no-ff --no-commit refs/upstream-tags/$T
git restore --source=refs/upstream-tags/$T --staged --worktree -- ':/' \
  ':(top,exclude)differ-crates' ':(top,exclude)differ-docs' ':(top,exclude)assets/themes/differ' \
  ':(top,exclude)script/differ-*' ':(top,exclude).github/workflows/differ-*' ':(top,exclude)THIRD_PARTY_NOTICES.md' \
  ':(top,exclude)README.md' ':(top,exclude).github/FUNDING.yml'
git restore --source=HEAD --staged --worktree -- README.md; git rm -q --ignore-unmatch .github/FUNDING.yml
git diff refs/upstream-tags/$OLD main -- Cargo.toml | git apply --index   # or re-add the members line by hand
echo $T > differ-docs/ZED_VERSION && git add differ-docs/ZED_VERSION
cargo metadata --format-version 1 >/dev/null && git add Cargo.lock
git diff --cached --stat refs/upstream-tags/$T   # only touch points + Differ paths
git commit -m "Merge Zed $T into Differ"
```

## Validation log (2026-10-27, throwaway `git clone --shared` in /tmp; the real repo was not touched)

- **Zed:**
  - Tag detection picked v1.25.3, which matches `gh releases/latest`.
  - The dry run from the real `main` predicted 5 conflicts, 3 of them Zed-owned, and flagged the rebaseline.
  - `--rebaseline` produced a tree equal to v1.25.3 plus README and FUNDING.yml.
  - Re-runs reported "already done" and then "up to date".
  - A simulated v1.24.4 base with the members line and a stub `differ` crate synced to v1.25.3: 7 natural conflicts, all resolved, and Cargo.toml was v1.25.3's plus exactly the members line.
  - A stale sync branch, and an edited Zed file (exit 4), were both caught. With no cargo installed the build exited 3, as designed.
- **T3:**
  - Preview and nightly tags were refused.
  - Pinned 0.1.1, then `--latest`, which picked 0.1.3: protocol 2 → 2, 5 checksums matching GitHub's digests, and the linux-x64 runtime downloaded and verified.
  - A simulated protocol 1 → 2 change stopped with exit 2, and a tampered checksum pin stopped with exit 2.
  - Codegen, build, and contract tests were skipped as placeholders. `regen.sh` doesn't exist yet, and this machine has no cargo.
- **Against branch `m0/t3-client` (27aff7e15d, another session's work, fetched read-only into the clone):**
  - Zed `--rebaseline` re-applied its 6-line members block onto v1.25.3 unchanged.
  - With the patch context broken on purpose, the fallback re-inserted the block after `members = [`.
  - The T3 sync read the key=value pin (0.1.1, protocol 2), rewrote it for 0.1.3 with the tag's commit, and checked `ORCHESTRATION_PROTOCOL_VERSION: u32 = 2` in `auth.rs`.
