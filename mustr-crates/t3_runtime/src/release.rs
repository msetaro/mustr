//! T3 Code's release archives: which one runs on this machine, where to download it, and the
//! checksum the release publishes for it (`mustr-docs/T3-RUNTIME.md` §2).

use anyhow::{Context as _, Result, bail};

/// Where T3 publishes its releases. `MUSTR_T3_RELEASE_BASE_URL` points at a mirror instead, as
/// `T3CODE_RELEASE_BASE_URL` does for T3's own installer.
pub const RELEASE_BASE_URL: &str = "https://github.com/pingdotgg/t3code/releases/download";
pub const RELEASE_BASE_URL_ENV: &str = "MUSTR_T3_RELEASE_BASE_URL";
pub const CHECKSUMS_FILE: &str = "SHA256SUMS";

pub fn release_base_url() -> String {
    std::env::var(RELEASE_BASE_URL_ENV)
        .ok()
        .map(|url| url.trim().trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| RELEASE_BASE_URL.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X64,
    Arm64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    TarGz,
    Zip,
}

/// A platform T3 publishes an engine archive for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub os: Os,
    pub arch: Arch,
}

impl Target {
    pub fn current() -> Result<Self> {
        Self::from_rust(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Maps Rust's `std::env::consts::{OS, ARCH}` the way T3's installer maps `uname`.
    pub fn from_rust(os: &str, arch: &str) -> Result<Self> {
        let os = match os {
            "linux" => Os::Linux,
            "macos" => Os::MacOs,
            "windows" => Os::Windows,
            other => bail!("T3 Code doesn't publish its agents engine for {other}"),
        };
        let arch = match arch {
            "x86_64" => Arch::X64,
            "aarch64" => Arch::Arm64,
            other => bail!("T3 Code doesn't publish its agents engine for {other} processors"),
        };
        if os == Os::MacOs && arch == Arch::X64 {
            // `.github/workflows/release.yml`: Node single-executable apps crash on Intel Macs, so
            // T3 ships no darwin-x64 archive.
            bail!(
                "T3 Code doesn't publish its agents engine for Intel Macs. Use mustr on an Apple \
                 silicon Mac, or connect it to a T3 Code server on another machine (set \"t3\": \
                 {{\"mode\": \"external\", \"url\": …}} in mustr's settings)."
            );
        }
        Ok(Self { os, arch })
    }

    fn platform(self) -> &'static str {
        match self.os {
            Os::Linux => "linux",
            Os::MacOs => "darwin",
            Os::Windows => "win32",
        }
    }

    fn arch_name(self) -> &'static str {
        match self.arch {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
        }
    }

    pub fn archive_kind(self) -> ArchiveKind {
        match self.os {
            Os::Windows => ArchiveKind::Zip,
            Os::Linux | Os::MacOs => ArchiveKind::TarGz,
        }
    }

    /// The archive's top-level directory, which T3's installer strips: `t3-<V>-<platform>-<arch>`.
    pub fn archive_stem(self, version: &str) -> String {
        format!("t3-{version}-{}-{}", self.platform(), self.arch_name())
    }

    pub fn asset_name(self, version: &str) -> String {
        let extension = match self.archive_kind() {
            ArchiveKind::TarGz => "tar.gz",
            ArchiveKind::Zip => "zip",
        };
        format!("{}.{extension}", self.archive_stem(version))
    }

    pub fn executable_name(self) -> &'static str {
        match self.os {
            Os::Windows => "t3.exe",
            Os::Linux | Os::MacOs => "t3",
        }
    }
}

/// A version mustr can put in a path: plain semver, so it can't name another directory.
pub fn validate_version(version: &str) -> Result<()> {
    semver::Version::parse(version)
        .map(drop)
        .with_context(|| format!("{version:?} isn't a T3 Code version"))
}

pub fn release_file_url(base_url: &str, version: &str, file: &str) -> String {
    format!("{}/v{version}/{file}", base_url.trim_end_matches('/'))
}

/// The SHA-256 that a `SHA256SUMS` file (`sha256sum` format, `<hex>  <name>` or
/// `<hex> *<name>`) lists for `asset`, in lowercase hex. Fails when the asset isn't listed, is
/// listed with conflicting sums, or its sum isn't one.
pub fn checksum_for(sha256sums: &str, asset: &str) -> Result<String> {
    let mut found: Option<String> = None;
    for line in sha256sums.lines() {
        let Some((sum, name)) = line.trim_end().split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        if name != asset {
            continue;
        }
        let sum = sum.to_ascii_lowercase();
        if sum.len() != 64 || !sum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("{CHECKSUMS_FILE} lists a malformed checksum for {asset}");
        }
        match &found {
            Some(previous) if *previous != sum => {
                bail!("{CHECKSUMS_FILE} lists conflicting checksums for {asset}")
            }
            _ => found = Some(sum),
        }
    }
    found.with_context(|| format!("{asset} isn't listed in the release's {CHECKSUMS_FILE}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_t3s_archive_for_each_platform() {
        let asset = |os, arch| Target::from_rust(os, arch).map(|target| target.asset_name("0.1.1"));
        assert_eq!(
            asset("linux", "x86_64").unwrap(),
            "t3-0.1.1-linux-x64.tar.gz"
        );
        assert_eq!(
            asset("linux", "aarch64").unwrap(),
            "t3-0.1.1-linux-arm64.tar.gz"
        );
        assert_eq!(
            asset("macos", "aarch64").unwrap(),
            "t3-0.1.1-darwin-arm64.tar.gz"
        );
        assert_eq!(
            asset("windows", "x86_64").unwrap(),
            "t3-0.1.1-win32-x64.zip"
        );
        assert_eq!(
            asset("windows", "aarch64").unwrap(),
            "t3-0.1.1-win32-arm64.zip"
        );
        let target = Target::from_rust("macos", "aarch64").unwrap();
        assert_eq!(target.archive_stem("0.1.1"), "t3-0.1.1-darwin-arm64");
        assert_eq!(target.executable_name(), "t3");
        assert_eq!(
            Target::from_rust("windows", "x86_64")
                .unwrap()
                .executable_name(),
            "t3.exe"
        );
    }

    #[test]
    fn platforms_without_an_archive_say_so() {
        let intel_mac = Target::from_rust("macos", "x86_64").unwrap_err();
        assert!(format!("{intel_mac:#}").contains("Intel Macs"));
        assert!(Target::from_rust("freebsd", "x86_64").is_err());
        assert!(Target::from_rust("linux", "riscv64").is_err());
    }

    #[test]
    fn this_machine_is_a_target_or_says_why_not() {
        // Every platform mustr's CI builds for, except Intel Macs, has an archive.
        let result = Target::current();
        if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            assert!(result.is_err());
        } else {
            assert!(result.is_ok(), "{result:?}");
        }
    }

    #[test]
    fn release_urls() {
        assert_eq!(
            release_file_url(RELEASE_BASE_URL, "0.1.1", "SHA256SUMS"),
            "https://github.com/pingdotgg/t3code/releases/download/v0.1.1/SHA256SUMS"
        );
        assert_eq!(
            release_file_url(
                "https://mirror.example/",
                "0.1.1",
                "t3-0.1.1-linux-x64.tar.gz"
            ),
            "https://mirror.example/v0.1.1/t3-0.1.1-linux-x64.tar.gz"
        );
    }

    #[test]
    fn versions_must_be_semver() {
        assert!(validate_version("0.1.1").is_ok());
        assert!(validate_version("0.1.4-nightly.20261027.3783").is_ok());
        assert!(validate_version("../0.1.1").is_err());
        assert!(validate_version("").is_err());
    }

    #[test]
    fn reads_checksums_in_sha256sum_format() {
        let sums = "\
33c43ea2f803215662f5a31fdf9e1bca86ab67cac4f2398565c2ba12e17c2185  t3-0.1.1-linux-x64.tar.gz
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA *t3-0.1.1-darwin-arm64.tar.gz
bbbb  t3-0.1.1-win32-x64.zip
";
        assert_eq!(
            checksum_for(sums, "t3-0.1.1-linux-x64.tar.gz").unwrap(),
            "33c43ea2f803215662f5a31fdf9e1bca86ab67cac4f2398565c2ba12e17c2185"
        );
        // Binary-mode marker and uppercase hex.
        assert_eq!(
            checksum_for(sums, "t3-0.1.1-darwin-arm64.tar.gz").unwrap(),
            "a".repeat(64)
        );
        assert!(checksum_for(sums, "t3-0.1.1-win32-x64.zip").is_err());
        assert!(checksum_for(sums, "t3-0.1.1-linux-arm64.tar.gz").is_err());
        // A name that merely ends with the asset's isn't the asset.
        assert!(
            checksum_for(
                &format!("{}  evil-t3-0.1.1-linux-x64.tar.gz\n", "c".repeat(64)),
                "t3-0.1.1-linux-x64.tar.gz"
            )
            .is_err()
        );
        let conflicting = format!(
            "{}  t3-0.1.1-linux-x64.tar.gz\n{}  t3-0.1.1-linux-x64.tar.gz\n",
            "c".repeat(64),
            "d".repeat(64)
        );
        assert!(checksum_for(&conflicting, "t3-0.1.1-linux-x64.tar.gz").is_err());
    }
}
