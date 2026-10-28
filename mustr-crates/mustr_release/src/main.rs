//! `mustr-release`: makes the update-signing key, and writes, signs, and checks a release's
//! update manifest (`mustr-docs/RELEASING.md`).

use anyhow::{Context as _, Result, bail, ensure};
use mustr_release::{
    Asset, Manifest, PLATFORM_MACOS_AARCH64, PublicKey, SIGNING_KEY_ENV, SigningKey, asset_name,
    release_notes_url, size_and_sha256, verified_manifest,
};
use semver::Version;
use std::{
    collections::BTreeMap,
    io::Write as _,
    path::{Path, PathBuf},
};

const USAGE: &str = "\
usage:
  mustr-release keygen <private-key-file>
      Makes an update-signing key pair. Writes the private key to the file (mode 0600; an
      existing file is never overwritten) and prints only the public key.
  mustr-release manifest --version <x.y.z> --zip <file> --published-at <rfc3339>
                         [--release-notes-url <url>] [--out <manifest.json>]
      Writes the update manifest for a macOS (Apple silicon) release zip.
  mustr-release sign <manifest.json> [--key-file <file>] [--out <manifest.json.sig>]
      Signs the manifest's exact bytes. The key comes from --key-file, or else from the
      MUSTR_UPDATE_SIGNING_KEY environment variable.
  mustr-release verify <manifest.json> --public-key-file <file> [--signature <file>]
      Checks the signature (default <manifest.json>.sig) with a public key file, as mustr does.
";

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match run(&arguments) {
        Ok(output) => print!("{output}"),
        Err(error) => {
            eprintln!("mustr-release: {error:#}");
            std::process::exit(1);
        }
    }
}

/// Runs a command and returns what it prints on stdout.
fn run(arguments: &[String]) -> Result<String> {
    let Some((command, rest)) = arguments.split_first() else {
        bail!("no command given\n{USAGE}");
    };
    let mut options = Options::parse(rest)?;
    let output = match command.as_str() {
        "keygen" => {
            let path = options.single_positional("the private key file")?;
            options.finish()?;
            keygen(&path)?
        }
        "manifest" => {
            let version = options.required("--version")?;
            let zip = PathBuf::from(options.required("--zip")?);
            let published_at = options.required("--published-at")?;
            let notes_url = options.take("--release-notes-url");
            let out = options.take("--out").map(PathBuf::from);
            options.finish()?;
            write_manifest(&version, &zip, &published_at, notes_url, out.as_deref())?
        }
        "sign" => {
            let manifest = options.single_positional("the manifest")?;
            let key_file = options.take("--key-file").map(PathBuf::from);
            let out = options.take("--out").map(PathBuf::from);
            options.finish()?;
            let key_text = match key_file {
                Some(path) => std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
                None => std::env::var(SIGNING_KEY_ENV).with_context(|| {
                    format!("give --key-file, or set {SIGNING_KEY_ENV} to the private key")
                })?,
            };
            sign(&manifest, &key_text, out.as_deref())?
        }
        "verify" => {
            let manifest = options.single_positional("the manifest")?;
            let public_key_file = PathBuf::from(options.required("--public-key-file")?);
            let signature = options.take("--signature").map(PathBuf::from);
            options.finish()?;
            verify(&manifest, &public_key_file, signature.as_deref())?
        }
        "help" | "--help" | "-h" => USAGE.to_string(),
        _ => bail!("unknown command {command:?}\n{USAGE}"),
    };
    Ok(output)
}

struct Options {
    named: BTreeMap<String, String>,
    positional: Vec<String>,
}

impl Options {
    fn parse(arguments: &[String]) -> Result<Self> {
        let mut named = BTreeMap::new();
        let mut positional = Vec::new();
        let mut arguments = arguments.iter();
        while let Some(argument) = arguments.next() {
            if argument.starts_with("--") {
                let value = arguments
                    .next()
                    .with_context(|| format!("{argument} needs a value"))?;
                ensure!(
                    named.insert(argument.clone(), value.clone()).is_none(),
                    "{argument} is given twice"
                );
            } else {
                positional.push(argument.clone());
            }
        }
        Ok(Self { named, positional })
    }

    fn take(&mut self, name: &str) -> Option<String> {
        self.named.remove(name)
    }

    fn required(&mut self, name: &str) -> Result<String> {
        self.take(name)
            .with_context(|| format!("{name} is required"))
    }

    fn single_positional(&mut self, what: &str) -> Result<PathBuf> {
        ensure!(self.positional.len() == 1, "give {what}, once\n{USAGE}");
        Ok(PathBuf::from(self.positional.remove(0)))
    }

    fn finish(self) -> Result<()> {
        if let Some(name) = self.named.keys().next() {
            bail!("unknown option {name}\n{USAGE}");
        }
        ensure!(
            self.positional.is_empty(),
            "unexpected argument {:?}\n{USAGE}",
            self.positional.join(" ")
        );
        Ok(())
    }
}

fn keygen(path: &Path) -> Result<String> {
    let (private_text, public_key) = SigningKey::generate()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {} (it must not exist yet)", path.display()))?;
    file.write_all(private_text.as_bytes())?;
    file.sync_all()?;
    eprintln!(
        "Wrote the private key to {}. Keep it secret: store it as the repository's \
         {SIGNING_KEY_ENV} secret, keep a backup somewhere safe, and delete this file. \
         Put the public key below in mustr-crates/mustr/resources/update-signing-key.pub.",
        path.display()
    );
    Ok(format!("{}\n", public_key.to_base64()))
}

fn write_manifest(
    version: &str,
    zip: &Path,
    published_at: &str,
    release_notes: Option<String>,
    out: Option<&Path>,
) -> Result<String> {
    let version = Version::parse(version.trim_start_matches('v'))
        .with_context(|| format!("{version:?} isn't a semantic version"))?;
    let name = asset_name(&version, PLATFORM_MACOS_AARCH64);
    ensure!(
        zip.file_name().is_some_and(|file_name| *file_name == *name),
        "the zip for mustr {version} must be named {name}, not {}",
        zip.display()
    );
    ensure!(!published_at.trim().is_empty(), "--published-at is empty");
    let file = std::fs::File::open(zip).with_context(|| format!("opening {}", zip.display()))?;
    let (size, sha256) = size_and_sha256(file)?;
    let manifest = Manifest {
        release_notes_url: release_notes.unwrap_or_else(|| release_notes_url(&version)),
        published_at: published_at.trim().to_string(),
        platforms: BTreeMap::from([(
            PLATFORM_MACOS_AARCH64.to_string(),
            Asset { name, size, sha256 },
        )]),
        version,
    };
    let json = manifest.to_json()?;
    // What mustr will read must parse.
    Manifest::parse(&json)?;
    match out {
        Some(out) => {
            std::fs::write(out, &json).with_context(|| format!("writing {}", out.display()))?;
            Ok(format!("Wrote {}\n", out.display()))
        }
        None => Ok(String::from_utf8(json)?),
    }
}

fn sign(manifest: &Path, key_text: &str, out: Option<&Path>) -> Result<String> {
    let bytes =
        std::fs::read(manifest).with_context(|| format!("reading {}", manifest.display()))?;
    Manifest::parse(&bytes).with_context(|| format!("checking {}", manifest.display()))?;
    let key = SigningKey::from_text(key_text)?;
    let signature = key.sign(&bytes);
    let out = out.map_or_else(|| signature_path(manifest), Path::to_path_buf);
    std::fs::write(&out, signature).with_context(|| format!("writing {}", out.display()))?;
    Ok(format!(
        "Signed {} with the key whose public half is {}; wrote {}\n",
        manifest.display(),
        key.public_key().to_base64(),
        out.display()
    ))
}

fn verify(manifest: &Path, public_key_file: &Path, signature: Option<&Path>) -> Result<String> {
    let key_text = std::fs::read_to_string(public_key_file)
        .with_context(|| format!("reading {}", public_key_file.display()))?;
    let Some(key) = PublicKey::from_file_text(&key_text)? else {
        bail!(
            "{} has no public key yet (it's the placeholder), so mustr built from it can't \
             update. See mustr-docs/RELEASING.md, \"The signing key\"",
            public_key_file.display()
        );
    };
    let signature = signature.map_or_else(|| signature_path(manifest), Path::to_path_buf);
    let manifest_bytes =
        std::fs::read(manifest).with_context(|| format!("reading {}", manifest.display()))?;
    let signature_bytes =
        std::fs::read(&signature).with_context(|| format!("reading {}", signature.display()))?;
    let parsed = verified_manifest(&manifest_bytes, &signature_bytes, &key)?;
    Ok(format!(
        "ok: {} is mustr {}, signed by {}\n",
        manifest.display(),
        parsed.version,
        key.to_base64()
    ))
}

fn signature_path(manifest: &Path) -> PathBuf {
    let mut path = manifest.as_os_str().to_owned();
    path.push(".sig");
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn arguments(text: &[&str]) -> Vec<String> {
        text.iter().map(|argument| argument.to_string()).collect()
    }

    fn path(dir: &Path, name: &str) -> String {
        dir.join(name).to_string_lossy().into_owned()
    }

    #[test]
    fn a_release_is_keyed_written_signed_and_verified() {
        let dir = tempfile::tempdir().unwrap();
        let key_file = path(dir.path(), "update.key");
        let public_key = run(&arguments(&["keygen", &key_file])).unwrap();
        let public_key = PublicKey::from_base64(public_key.trim()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&key_file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Never overwritten, so a second keygen can't destroy the key in use.
        let error = run(&arguments(&["keygen", &key_file])).unwrap_err();
        assert!(
            format!("{error:#}").contains("must not exist yet"),
            "{error:#}"
        );

        let zip = path(dir.path(), "mustr-0.2.0-macos-aarch64.zip");
        std::fs::write(&zip, b"test").unwrap();
        let manifest = path(dir.path(), "manifest.json");
        run(&arguments(&[
            "manifest",
            "--version",
            "0.2.0",
            "--zip",
            &zip,
            "--published-at",
            "2026-10-28T12:00:00Z",
            "--out",
            &manifest,
        ]))
        .unwrap();
        let written = Manifest::parse(&std::fs::read(&manifest).unwrap()).unwrap();
        let asset = written.asset(PLATFORM_MACOS_AARCH64).unwrap();
        assert_eq!(
            (asset.name.as_str(), asset.size, asset.sha256.as_str()),
            (
                "mustr-0.2.0-macos-aarch64.zip",
                4,
                "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
            )
        );
        assert_eq!(
            written.release_notes_url,
            "https://github.com/msetaro/mustr/releases/tag/v0.2.0"
        );

        let output = run(&arguments(&["sign", &manifest, "--key-file", &key_file])).unwrap();
        assert!(
            output.contains(&public_key.to_base64()),
            "it names the public key: {output}"
        );
        let private_key = std::fs::read_to_string(&key_file).unwrap();
        assert!(!output.contains(private_key.trim()), "{output}");

        let public_key_file = path(dir.path(), "update-signing-key.pub");
        std::fs::write(
            &public_key_file,
            format!("# comment\n{}\n", public_key.to_base64()),
        )
        .unwrap();
        let output = run(&arguments(&[
            "verify",
            &manifest,
            "--public-key-file",
            &public_key_file,
        ]))
        .unwrap();
        assert!(output.starts_with("ok: "), "{output}");
        assert!(output.contains("is mustr 0.2.0"), "{output}");

        // A placeholder key file can't verify anything.
        std::fs::write(&public_key_file, "# placeholder\n").unwrap();
        let error = run(&arguments(&[
            "verify",
            &manifest,
            "--public-key-file",
            &public_key_file,
        ]))
        .unwrap_err();
        assert!(format!("{error:#}").contains("placeholder"), "{error:#}");
    }

    #[test]
    fn manifests_must_name_the_zip_for_their_version() {
        let dir = tempfile::tempdir().unwrap();
        let zip = path(dir.path(), "mustr-0.1.9-macos-aarch64.zip");
        std::fs::write(&zip, b"test").unwrap();
        let error = run(&arguments(&[
            "manifest",
            "--version",
            "v0.2.0",
            "--zip",
            &zip,
            "--published-at",
            "2026-10-28T12:00:00Z",
        ]))
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("must be named mustr-0.2.0-macos-aarch64.zip"),
            "{error:#}"
        );
    }

    #[test]
    fn signing_refuses_a_bad_key_or_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = path(dir.path(), "manifest.json");
        std::fs::write(&manifest, b"{}").unwrap();
        assert!(sign(Path::new(&manifest), "irrelevant", None).is_err());

        let error = run(&arguments(&["sign"])).unwrap_err();
        assert!(
            format!("{error:#}").contains("give the manifest"),
            "{error:#}"
        );
        let error = run(&arguments(&["sign", &manifest, "--key", "x"])).unwrap_err();
        assert!(
            format!("{error:#}").contains("unknown option --key"),
            "{error:#}"
        );
    }
}
