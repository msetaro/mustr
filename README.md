# mustr

A desktop app for reviewing pull requests: reviewer first, with a full IDE underneath.

Most editors are built for writing code and add review as a plugin. mustr is the reverse. The review is the home screen, and the editor's tools (go to definition, references, project search, a terminal) are one cmd+click away from any line of the diff.

> [!NOTE]
> **Status: just forked.** This repository is currently an unmodified copy of [Zed](https://github.com/zed-industries/zed) and still builds as Zed. The review-first app is being built on top of it, starting with GitHub.

### Relationship to Zed

mustr is a fork of [Zed](https://github.com/zed-industries/zed), the editor made by Zed Industries, Inc. It is not affiliated with or endorsed by Zed Industries. Zed's name and logo belong to Zed Industries and will be removed as mustr is rebranded. mustr tracks upstream Zed and merges its releases.

### Building

Until mustr has its own build docs, follow Zed's:

- [Building on macOS](./docs/src/development/macos.md)
- [Building on Linux](./docs/src/development/linux.md)
- [Building on Windows](./docs/src/development/windows.md)

### Licensing

mustr is licensed under GPL-3.0-or-later ([LICENSE-GPL](./LICENSE-GPL)), the same as Zed. Some crates inherited from Zed, including `gpui`, are Apache-2.0 ([LICENSE-APACHE](./LICENSE-APACHE)). Each crate's `Cargo.toml` states its license.

Third-party dependency licenses are checked with [`cargo-about`](https://github.com/EmbarkStudios/cargo-about). If that check fails:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable, add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).
