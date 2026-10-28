//! mustr's update manifest and its signature.
//!
//! Each release publishes `manifest.json`, which names the release's version and, for each
//! platform, its download's file name, size, and SHA-256. Next to it is `manifest.json.sig`, an
//! Ed25519 signature over the manifest's exact bytes, in base64, the way Sparkle signs its
//! appcasts (`sparkle:edSignature`). The release workflow signs with the private key in the
//! `MUSTR_UPDATE_SIGNING_KEY` secret, and mustr checks the signature with the public key it's
//! built with (`mustr-crates/mustr/resources/update-signing-key.pub`) before it reads anything in
//! the manifest. `mustr-docs/RELEASING.md` describes the keys.

use anyhow::{Context as _, Result, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, io::Read};

pub const MANIFEST_FILE: &str = "manifest.json";
pub const SIGNATURE_FILE: &str = "manifest.json.sig";
/// Apple silicon Macs, the only platform mustr publishes so far.
pub const PLATFORM_MACOS_AARCH64: &str = "macos-aarch64";
/// The environment variable `mustr-release sign` reads the private key from (the GitHub Actions
/// secret of the same name).
pub const SIGNING_KEY_ENV: &str = "MUSTR_UPDATE_SIGNING_KEY";
/// The repository releases are published to.
pub const REPOSITORY: &str = "msetaro/mustr";

/// The file name of a release's download for `platform`: `mustr-0.2.0-macos-aarch64.zip`.
pub fn asset_name(version: &Version, platform: &str) -> String {
    format!("mustr-{version}-{platform}.zip")
}

/// The release page a manifest links to by default.
pub fn release_notes_url(version: &Version) -> String {
    format!("https://github.com/{REPOSITORY}/releases/tag/v{version}")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: Version,
    /// When the release was published, in RFC 3339 (UTC).
    pub published_at: String,
    pub release_notes_url: String,
    /// The downloads, by platform ([`PLATFORM_MACOS_AARCH64`]).
    pub platforms: BTreeMap<String, Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// The file name of the release asset, next to the manifest.
    pub name: String,
    /// In bytes.
    pub size: u64,
    /// In lowercase hex.
    pub sha256: String,
}

impl Manifest {
    /// Parses a manifest. Only call this on bytes whose signature [`verify`] accepted; use
    /// [`verified_manifest`] for both.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let manifest: Self =
            serde_json::from_slice(bytes).context("the update manifest isn't valid")?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.platforms.is_empty(),
            "the update manifest lists no downloads"
        );
        for (platform, asset) in &self.platforms {
            // The updater downloads the name next to the manifest and saves it under that name.
            ensure!(
                is_plain_file_name(&asset.name),
                "the update manifest's {platform} download has an invalid name, {:?}",
                asset.name
            );
            ensure!(
                is_sha256_hex(&asset.sha256),
                "the update manifest's {platform} download has an invalid SHA-256, {:?}",
                asset.sha256
            );
            ensure!(
                asset.size > 0,
                "the update manifest's {platform} download is empty"
            );
        }
        Ok(())
    }

    pub fn asset(&self, platform: &str) -> Option<&Asset> {
        self.platforms.get(platform)
    }

    /// The manifest as the release publishes it: pretty-printed JSON with a final newline. The
    /// signature covers these bytes exactly.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        let mut json = serde_json::to_vec_pretty(self)?;
        json.push(b'\n');
        Ok(json)
    }
}

fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-+".contains(character))
}

fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// An Ed25519 public key: the 32 raw bytes, written in base64 (as Sparkle's `SUPublicEDKey`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    /// Reads a `.pub` file: the key in base64 on a line of its own, and `#` comment lines.
    /// Returns `None` when there's no key line, as in the placeholder mustr ships until the
    /// release key is made.
    pub fn from_file_text(text: &str) -> Result<Option<Self>> {
        let mut lines = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'));
        let Some(key) = lines.next() else {
            return Ok(None);
        };
        ensure!(
            lines.next().is_none(),
            "the public key file has more than one key line"
        );
        Self::from_base64(key).map(Some)
    }

    pub fn from_base64(text: &str) -> Result<Self> {
        let bytes = BASE64
            .decode(text.trim())
            .context("the public key isn't valid base64")?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
            anyhow!(
                "an Ed25519 public key has 32 bytes, and this one has {}",
                bytes.len()
            )
        })?;
        Ok(Self(bytes))
    }

    pub fn to_base64(&self) -> String {
        BASE64.encode(self.0)
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "PublicKey({})", self.to_base64())
    }
}

/// Checks `signature` (a `.sig` file: base64, surrounding whitespace allowed) over `message`.
pub fn verify(message: &[u8], signature: &[u8], key: &PublicKey) -> Result<()> {
    let signature = std::str::from_utf8(signature)
        .ok()
        .and_then(|text| BASE64.decode(text.trim()).ok())
        .filter(|bytes| bytes.len() == 64)
        .context("the update manifest's signature isn't a base64 Ed25519 signature")?;
    UnparsedPublicKey::new(&ED25519, key.0)
        .verify(message, &signature)
        .map_err(|_| anyhow!("the update manifest's signature doesn't match mustr's signing key"))
}

/// Checks the manifest's signature, and only then parses it.
pub fn verified_manifest(manifest: &[u8], signature: &[u8], key: &PublicKey) -> Result<Manifest> {
    verify(manifest, signature, key)?;
    Manifest::parse(manifest)
}

/// The private half. Its `Debug` shows only the public half, so it can't end up in a log.
pub struct SigningKey(Ed25519KeyPair);

impl fmt::Debug for SigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SigningKey(public: {})",
            self.public_key().to_base64()
        )
    }
}

impl SigningKey {
    /// Makes a new key pair. The private key's text is its PKCS #8 document in base64, on one
    /// line.
    pub fn generate() -> Result<(String, PublicKey)> {
        let random = ring::rand::SystemRandom::new();
        let document = Ed25519KeyPair::generate_pkcs8(&random)
            .map_err(|_| anyhow!("couldn't generate a key pair"))?;
        let text = format!("{}\n", BASE64.encode(document.as_ref()));
        let key = Self::from_text(&text)?;
        Ok((text, key.public_key()))
    }

    /// Reads what [`SigningKey::generate`] wrote (surrounding whitespace allowed).
    pub fn from_text(text: &str) -> Result<Self> {
        let document = BASE64
            .decode(text.trim())
            .map_err(|_| anyhow!("the signing key isn't valid base64"))?;
        Ed25519KeyPair::from_pkcs8(&document)
            .map(Self)
            .map_err(|_| anyhow!("the signing key isn't an Ed25519 PKCS #8 key"))
    }

    pub fn public_key(&self) -> PublicKey {
        let mut bytes = [0; 32];
        bytes.copy_from_slice(self.0.public_key().as_ref());
        PublicKey(bytes)
    }

    /// The signature over `message`, as a `.sig` file holds it: base64 and a newline.
    pub fn sign(&self, message: &[u8]) -> String {
        format!("{}\n", BASE64.encode(self.0.sign(message).as_ref()))
    }
}

/// The size and SHA-256 (lowercase hex) of everything `reader` reads.
pub fn size_and_sha256(mut reader: impl Read) -> Result<(u64, String)> {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0; 256 * 1024];
    let mut size = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((size, hex::encode(context.finish())))
}

/// A SHA-256 being computed piece by piece, for downloads.
pub struct Sha256(ring::digest::Context);

impl Default for Sha256 {
    fn default() -> Self {
        Self(ring::digest::Context::new(&ring::digest::SHA256))
    }
}

impl Sha256 {
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// In lowercase hex.
    pub fn finish(self) -> String {
        hex::encode(self.0.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn manifest() -> Manifest {
        let version = Version::new(0, 2, 0);
        Manifest {
            platforms: BTreeMap::from([(
                PLATFORM_MACOS_AARCH64.to_string(),
                Asset {
                    name: asset_name(&version, PLATFORM_MACOS_AARCH64),
                    size: 48_211_337,
                    sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
                        .to_string(),
                },
            )]),
            release_notes_url: release_notes_url(&version),
            published_at: "2026-10-28T12:00:00Z".to_string(),
            version,
        }
    }

    #[test]
    fn manifests_round_trip_in_the_published_format() {
        let json = manifest().to_json().unwrap();
        assert_eq!(
            String::from_utf8(json.clone()).unwrap(),
            r#"{
  "version": "0.2.0",
  "published_at": "2026-10-28T12:00:00Z",
  "release_notes_url": "https://github.com/msetaro/mustr/releases/tag/v0.2.0",
  "platforms": {
    "macos-aarch64": {
      "name": "mustr-0.2.0-macos-aarch64.zip",
      "size": 48211337,
      "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
    }
  }
}
"#
        );
        assert_eq!(Manifest::parse(&json).unwrap(), manifest());
    }

    #[test]
    fn manifests_from_newer_releases_can_add_fields() {
        let json = br#"{
            "version": "0.3.0-beta.1", "published_at": "2026-11-02T08:30:00Z",
            "release_notes_url": "https://example.com/notes", "minimum_macos": "14.0",
            "platforms": {
                "macos-aarch64": { "name": "mustr-0.3.0-beta.1-macos-aarch64.zip", "size": 1,
                    "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
                    "notarized": true },
                "linux-x86_64": { "name": "mustr-0.3.0-beta.1-linux-x86_64.zip", "size": 1,
                    "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08" }
            }
        }"#;
        let manifest = Manifest::parse(json).unwrap();
        assert_eq!(manifest.version, Version::parse("0.3.0-beta.1").unwrap());
        assert_eq!(
            manifest.asset(PLATFORM_MACOS_AARCH64).unwrap().name,
            "mustr-0.3.0-beta.1-macos-aarch64.zip"
        );
    }

    #[test]
    fn invalid_manifests_are_refused() {
        let with = |change: &dyn Fn(&mut serde_json::Value)| {
            let mut json = serde_json::to_value(manifest()).unwrap();
            change(&mut json);
            Manifest::parse(&serde_json::to_vec(&json).unwrap())
                .unwrap_err()
                .to_string()
        };
        let asset = |json: &mut serde_json::Value| -> serde_json::Value {
            json["platforms"]["macos-aarch64"].take()
        };
        assert_eq!(
            with(&|json| json["version"] = "two".into()),
            "the update manifest isn't valid"
        );
        assert_eq!(
            with(&|json| {
                let mut entry = asset(json);
                entry["name"] = "../../.zshrc".into();
                json["platforms"]["macos-aarch64"] = entry;
            }),
            "the update manifest's macos-aarch64 download has an invalid name, \"../../.zshrc\""
        );
        assert_eq!(
            with(&|json| {
                let mut entry = asset(json);
                entry["sha256"] = "9F86".into();
                json["platforms"]["macos-aarch64"] = entry;
            }),
            "the update manifest's macos-aarch64 download has an invalid SHA-256, \"9F86\""
        );
        assert_eq!(
            with(&|json| {
                let mut entry = asset(json);
                entry["size"] = 0.into();
                json["platforms"]["macos-aarch64"] = entry;
            }),
            "the update manifest's macos-aarch64 download is empty"
        );
        assert_eq!(
            with(&|json| json["platforms"] = serde_json::json!({})),
            "the update manifest lists no downloads"
        );
        assert!(Manifest::parse(b"not json").is_err());
    }

    #[test]
    fn signatures_verify_with_the_matching_key_only() {
        let (private_text, public_key) = SigningKey::generate().unwrap();
        let key = SigningKey::from_text(&private_text).unwrap();
        assert_eq!(key.public_key(), public_key);
        let json = manifest().to_json().unwrap();
        let signature = key.sign(&json);
        assert!(signature.ends_with('\n'));

        assert_eq!(
            verified_manifest(&json, signature.as_bytes(), &public_key).unwrap(),
            manifest()
        );

        // One changed byte anywhere: a tampered manifest, even one that still parses.
        let tampered = String::from_utf8(json.clone())
            .unwrap()
            .replace("48211337", "48211338");
        assert_eq!(
            verified_manifest(tampered.as_bytes(), signature.as_bytes(), &public_key)
                .unwrap_err()
                .to_string(),
            "the update manifest's signature doesn't match mustr's signing key"
        );
        // Whitespace counts too: the signature is over the exact bytes.
        let mut reformatted = json.clone();
        reformatted.push(b'\n');
        assert!(verify(&reformatted, signature.as_bytes(), &public_key).is_err());

        // Signed with some other key.
        let (_, other_public_key) = SigningKey::generate().unwrap();
        assert_eq!(
            verify(&json, signature.as_bytes(), &other_public_key)
                .unwrap_err()
                .to_string(),
            "the update manifest's signature doesn't match mustr's signing key"
        );

        for garbage in [&b""[..], b"not base64!", b"c2hvcnQ=", &[0xff, 0xfe]] {
            assert_eq!(
                verify(&json, garbage, &public_key).unwrap_err().to_string(),
                "the update manifest's signature isn't a base64 Ed25519 signature"
            );
        }
    }

    #[test]
    fn public_key_files_hold_one_key_or_none() {
        let (_, public_key) = SigningKey::generate().unwrap();
        let file = format!(
            "# mustr's update-signing key\n\n  {}  \n# trailing comment\n",
            public_key.to_base64()
        );
        assert_eq!(PublicKey::from_file_text(&file).unwrap(), Some(public_key));
        assert_eq!(
            PublicKey::from_file_text("# a placeholder\n#\n# no key yet\n").unwrap(),
            None
        );
        assert_eq!(PublicKey::from_file_text("").unwrap(), None);
        assert!(PublicKey::from_file_text("c2hvcnQ=\n").is_err());
        assert!(PublicKey::from_file_text("not base64").is_err());
        let two = format!("{0}\n{0}\n", public_key.to_base64());
        assert!(PublicKey::from_file_text(&two).is_err());
    }

    #[test]
    fn invalid_signing_keys_are_refused_without_echoing_them() {
        let error = SigningKey::from_text("c2VjcmV0").unwrap_err().to_string();
        assert_eq!(error, "the signing key isn't an Ed25519 PKCS #8 key");
        let error = SigningKey::from_text("secret!").unwrap_err().to_string();
        assert_eq!(error, "the signing key isn't valid base64");
    }

    #[test]
    fn sizes_and_hashes_cover_every_byte() {
        let (size, sha256) = size_and_sha256(&b"test"[..]).unwrap();
        assert_eq!(size, 4);
        assert_eq!(
            sha256,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
        let mut streaming = Sha256::default();
        streaming.update(b"te");
        streaming.update(b"st");
        assert_eq!(streaming.finish(), sha256);
    }
}
