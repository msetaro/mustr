# Releasing mustr

How a mustr release is cut, what the release workflow does, how Macs install and update mustr,
and the key that signs updates. Releases are for Apple silicon Macs only, for now.

## What a release is

A GitHub release on `msetaro/mustr`, tagged `vX.Y.Z`, with five files:

| File | What it is |
|---|---|
| `mustr-X.Y.Z-macos-aarch64.zip` | `mustr.app`, a release build, signed ad hoc, zipped with `ditto` |
| `manifest.json` | The version, publish date, release-notes URL, and the zip's name, size, and SHA-256 |
| `manifest.json.sig` | An Ed25519 signature over `manifest.json`'s exact bytes, in base64 |
| `install.sh` | The installer, `script/mustr-install` |
| `SHA256SUMS` | Checksums of the other four |

- **First install** on a Mac, in Terminal:
  `curl -fsSL https://github.com/msetaro/mustr/releases/latest/download/install.sh | sh`.
  It installs `/Applications/mustr.app`, or `~/Applications/mustr.app` when `/Applications` isn't
  writable (`mustr-docs/MACOS.md`, "Install a release").
- **Updates:** mustr checks `releases/latest/download/manifest.json` 20 seconds after it starts
  and every 4 hours, and installs a newer release on its own; the status bar then offers
  **Restart to update**. mustr › Check for Updates… checks right away
  (`mustr-crates/mustr/src/updates.rs`).
- **No Apple Developer ID yet.** The app is signed ad hoc. curl doesn't mark what it downloads as
  quarantined, so Gatekeeper doesn't stop it. A zip downloaded in a browser is quarantined, and
  macOS refuses to open the app in it. See "Adding a Developer ID" for when that changes.

## Cutting a release

1. **Bump the version** in `mustr-crates/mustr/Cargo.toml` (`version = "0.2.0"`), and run
   `cargo check -p mustr` so `Cargo.lock` follows. Versions are semver; mustr compares them by
   semver precedence, so a release must be higher than the one before it.
2. **Commit on `m3/next`**, for example `mustr: Release 0.2.0`, and push the branch.
3. **Tag that commit and push the tag:**
   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```
   The tag must be `v` plus the version in `Cargo.toml` exactly; the workflow checks.
4. **Watch the run:** Actions › mustr release. It takes a while on a cold cache.
5. **Check the release:** its page lists the five files. On a Mac, run the install command, or
   choose mustr › Check for Updates… in an older mustr.

To undo a bad release, delete the GitHub release (or mark it as a prerelease), so "latest" points
at the one before. Macs that already updated keep the bad version until a newer one is out:
mustr never downgrades.

## What the workflow does

`.github/workflows/mustr-release.yml` runs only when a `v*.*.*` tag is pushed, or by hand
(**Run workflow**) for a dry run. It isn't CI, and it uses none of Zed's workflows. Every
action is pinned to a full commit SHA.

1. **Build job** (`macos-15`, Apple silicon; it can read the repository but not write to it):
   1. Checks out the tag and checks that the tag is `v` plus `mustr-crates/mustr/Cargo.toml`'s
      version.
   2. Installs the Rust toolchain that `rust-toolchain.toml` pins, and checks that `cmake` is
      there (the build needs it: `mustr-docs/MACOS.md`).
   3. Restores the cargo cache (`actions/cache`).
   4. Runs `script/mustr-package-mac`: a release build of `mustr` without debug info, wrapped in
      `mustr.app` by `script/mustr-bundle-mac`, signed ad hoc
      (`codesign --force --sign - --identifier dev.mustr.mustr`), zipped with
      `ditto -c -k --keepParent`, then unzipped again to check the signature, bundle ID, and
      version.
   5. Writes `manifest.json` with `mustr-release manifest` (`mustr-crates/mustr_release`).
   6. Signs it with `mustr-release sign`, which reads the `MUSTR_UPDATE_SIGNING_KEY` secret
      (only this step sees it), then checks the signature against
      `mustr-crates/mustr/resources/update-signing-key.pub`, as mustr will. A release whose
      signature doesn't verify with the committed public key fails here.
   7. Adds `install.sh` and `SHA256SUMS`, and uploads the five files as the workflow artifact
      `mustr-X.Y.Z-release`.
2. **Publish job** (`ubuntu-latest`, `contents: write`; it never sees the signing key): for a
   tag, downloads the artifact, checks `SHA256SUMS`, and runs `gh release create` with the five
   files, the install command, and GitHub's generated notes. A version with a `-` is published as
   a prerelease.

**Dry run.** Actions › mustr release › Run workflow, with "dry run" checked (the default), or
`gh workflow run mustr-release.yml --ref <branch> -f dry_run=true`. It builds and signs (if the
secret is set) and uploads the artifact, and publishes nothing. GitHub runs a manual workflow only
once its file is on the default branch (`main`). Download the artifact from the run's page to try
the zip on a Mac (unzip it with `ditto -x -k` in Terminal, or Finder will quarantine it).

## Prereleases

A version with a prerelease part, like `0.3.0-beta.1` (tag `v0.3.0-beta.1`), is published as a
GitHub prerelease. Prereleases are never "latest", so the installer and mustr's updater don't see
them. To install one: `curl -fsSL
https://github.com/msetaro/mustr/releases/download/v0.3.0-beta.1/install.sh | MUSTR_VERSION=0.3.0-beta.1 sh`.
That Mac then updates to the next stable release that's newer (0.3.0, not 0.3.0-beta.2).

## The signing key

mustr's updater reads a release's manifest only once its signature checks out with the public
key mustr was built with, so an update can't come from anywhere but the holder of the private
key. The installer can't check it (macOS has no Ed25519 tool), so a first install relies on HTTPS
and GitHub, like any `curl | sh`.

- **Private key:** only in the repository's Actions secret `MUSTR_UPDATE_SIGNING_KEY`, plus an
  offline backup (a password manager). Never commit it.
- **Public key:** `mustr-crates/mustr/resources/update-signing-key.pub`, compiled into mustr. While
  it's the placeholder (comment lines, no key), mustr's updater is off and logs
  `Updates: off, because this build has no update-signing key`, and the release workflow
  refuses to publish.

**Make the key once, before the first release** (the first release a Mac installs must already
have the real public key, or that Mac never updates and needs the install command again):

```sh
cargo run -p mustr_release -- keygen ~/mustr-update-signing.key   # prints the public key only
```

1. Put the printed public key on its own line in
   `mustr-crates/mustr/resources/update-signing-key.pub` (keep the comments), and commit.
2. Store the private key as the secret, and keep a backup:
   ```sh
   gh secret set MUSTR_UPDATE_SIGNING_KEY --repo msetaro/mustr < ~/mustr-update-signing.key
   ```
3. Delete `~/mustr-update-signing.key` once it's backed up.

`keygen` writes the file with mode 0600 and never overwrites one. To check a signature by hand:
`cargo run -p mustr_release -- verify manifest.json --public-key-file
mustr-crates/mustr/resources/update-signing-key.pub`.

**If the private key leaks.** On its own, it isn't enough to ship an update: the updater
downloads only from this repository's latest release, so an attacker would also need to publish
there. Still, replace it:

1. Revoke what leaked: check the repository's releases, Actions secrets, and collaborators for
   anything unexpected, and rotate any GitHub tokens involved.
2. Make a new key (above), commit the new public key, and set the secret to the new private key.
3. Installed copies trust only the old key, so the release with the new public key has to be
   signed with the old key for them to take it. If you still trust the old key for one last
   signature, do the release's dry run, sign its `manifest.json` by hand with the old key
   (`mustr-release sign manifest.json --key-file <old key>`), and publish the files by hand with
   `gh release create`. Otherwise, ask everyone to run the install command again.

## Adding a Developer ID later

If the work laptop's device management blocks apps that aren't from an identified developer, or
to install from a browser download, mustr needs Apple's Developer ID signing and notarization.

1. Join the Apple Developer Program, and create a **Developer ID Application** certificate.
   Export it with its private key as a `.p12`.
2. Add Actions secrets: the certificate (base64) and its password, and an App Store Connect API
   key for `notarytool` (key, key ID, issuer ID).
3. In the build job, before packaging, import the certificate into a temporary keychain (as
   Zed's `script/bundle-mac` does with `security create-keychain`, `security import`, and
   `security set-key-partition-list`), and set `MUSTR_CODESIGN_IDENTITY` to
   `Developer ID Application: <name> (<team ID>)`.
4. In `script/mustr-package-mac`, at the marked "Signing" spot: `--options runtime` and
   `--timestamp` already come with an identity. Add `--entitlements` with a new
   `mustr-crates/mustr/resources/mustr.entitlements`: the hardened runtime blocks the JIT that
   WebAssembly grammars use, so it needs `com.apple.security.cs.allow-jit` and
   `com.apple.security.cs.allow-unsigned-executable-memory`, as Zed's
   `crates/zed/resources/zed.entitlements` has.
5. Notarize after zipping (`xcrun notarytool submit <zip> --wait`), staple the app
   (`xcrun stapler staple mustr.app`), and zip again.

The updater needs no change: `ditto` keeps the signature, and mustr checks it with `codesign
--verify` before installing an update.

## Zed's workflows

The fork carries Zed's 49 workflow files, and GitHub enables each new one it sees. Before turning
Actions on, and after each Zed sync, run `script/mustr-disable-zed-workflows` (a dry run that
lists them), then `script/mustr-disable-zed-workflows --apply` with admin rights. It leaves
`.github/workflows/mustr-*` on.
