//! The self-updater: check GitHub for a newer release, verify its signature,
//! replace this binary.
//!
//! mcpurge is a tool that kills processes and deletes files, so a program that
//! can replace it with code fetched off the internet deserves more suspicion
//! than the feature usually gets. Three rules follow from that, and the code
//! below is mostly their enforcement:
//!
//! 1. **Nothing is installed that the release key did not sign.** The public
//!    key is compiled into the binary, not fetched, so an attacker who controls
//!    the download also has to have the private key. The signature is checked
//!    in memory, before a single byte is written anywhere executable.
//!
//!    The format is [minisign](https://jedisct1.github.io/minisign/), which is
//!    Ed25519 and nothing else - there is no algorithm field to be talked down
//!    to something weaker. The same signature files can be checked by hand
//!    with the `minisign` command, so the guarantee is not one you have to
//!    take this module's word for.
//! 2. **Nothing is installed without being asked.** The check can be automatic;
//!    the install never is. There is no silent update path in this module, and
//!    [`install`] is only ever reached from a button.
//! 3. **It refuses to overwrite a build.** Running out of `target/release`
//!    during development is normal, and an update that clobbered a cargo build
//!    directory would be a nasty surprise. [`Location::of_current_exe`] spots
//!    that case and the install is declined with an explanation.
//!
//! The toggle the user sees controls rule 1's *trigger* only: with automatic
//! checks off, mcpurge makes no network request unless asked to, which keeps
//! "this tool talks to nothing" true for anyone who wants it to be.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::Deserialize;

/// The key releases are signed with, compiled in.
///
/// Shipped in the binary rather than fetched, because a key you download from
/// the same place as the file it vouches for vouches for nothing.
pub const SIGNING_KEY: &str = include_str!("../assets/release-signing-key.pub");

/// The key ID [`SIGNING_KEY`] must carry. Checked by a test, so swapping the
/// key file without meaning to fails the build rather than the update.
pub const SIGNING_KEY_ID: &str = "0E85F817963BF7B5";

/// The suffix minisign gives a detached signature, and so the name of the
/// second asset every release has to carry.
const SIGNATURE_SUFFIX: &str = ".minisig";

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/TeeboBaggens/mcpurge/releases/latest";

/// GitHub asks for a User-Agent and answers 403 without one.
const USER_AGENT: &str = concat!("mcpurge/", env!("CARGO_PKG_VERSION"));

/// A ceiling on what will be pulled into memory. The binary is around 8 MB;
/// anything near this is not a build of mcpurge.
const MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 16 * 1024;
const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;

const TIMEOUT: Duration = Duration::from_secs(30);

/// The version this binary was built as.
pub fn current_version() -> Version {
    // A malformed version here is a build-time mistake, not a runtime one, and
    // the test below keeps it that way.
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version::new(0, 0, 0))
}

/// The release asset this platform wants.
///
/// Returns `None` on a platform releases are not built for, which is how an
/// unsupported target gets a plain "no build for your platform" instead of a
/// download that could not possibly be right.
pub fn asset_name() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "mcpurge-aarch64-apple-darwin",
        ("macos", "x86_64") => "mcpurge-x86_64-apple-darwin",
        ("linux", "x86_64") => "mcpurge-x86_64-unknown-linux-gnu",
        ("windows", "x86_64") => "mcpurge-x86_64-pc-windows-msvc.exe",
        _ => return None,
    })
}

/// Where this binary is running from, which decides whether replacing it is a
/// sensible thing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A normal install. Replacing it is what the updater is for.
    Installed(PathBuf),
    /// Inside a cargo `target/` directory: a build, not an install.
    CargoTarget(PathBuf),
}

impl Location {
    pub fn of_current_exe() -> Result<Self> {
        let exe = std::env::current_exe().context("could not find this executable's path")?;
        Ok(Self::of(&exe))
    }

    /// Split out from [`Location::of_current_exe`] so the rule can be tested
    /// against paths that are not this test binary's own.
    pub fn of(exe: &Path) -> Self {
        // A cargo build directory is a `target` directory with a manifest
        // beside it. Both halves are needed: `target` alone is a plausible
        // name for an install directory, and a stray Cargo.toml somewhere up
        // the tree says nothing on its own.
        let mut dir = exe.parent();
        while let Some(d) = dir {
            if d.file_name().is_some_and(|n| n == "target")
                && d.parent().is_some_and(|p| p.join("Cargo.toml").is_file())
            {
                return Location::CargoTarget(exe.to_path_buf());
            }
            dir = d.parent();
        }
        Location::Installed(exe.to_path_buf())
    }

    pub fn path(&self) -> &Path {
        match self {
            Location::Installed(p) | Location::CargoTarget(p) => p,
        }
    }
}

/// A release newer than this binary, and where to get it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub version: Version,
    pub tag: String,
    pub notes: String,
    pub binary_url: String,
    pub signature_url: String,
    pub size_bytes: u64,
}

impl fmt::Display for Available {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mcpurge {}", self.version)
    }
}

/// The slice of GitHub's release JSON that matters here.
#[derive(Debug, Deserialize)]
struct ReleaseJson {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Debug, Deserialize)]
struct AssetJson {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(USER_AGENT)
        // ureq defaults to rustls even when rustls is not compiled in, which
        // fails at the first https request rather than at build time. Saying
        // it out loud is what makes "the platform's own TLS" true.
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                // And the machine's roots, rather than ureq's default bundled
                // copy of Mozilla's - which is not compiled in here anyway, so
                // leaving this at its default panics on the first request.
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}

/// Ask GitHub for the latest release.
///
/// `Ok(None)` means there is nothing to do: this binary is current, or newer
/// than anything published, which is the normal answer and not an error.
pub fn check() -> Result<Option<Available>> {
    let response = match agent()
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(response) => response,
        // No releases published yet. That is an answer, not a failure, and it
        // is the answer for every repository until the first tag.
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(anyhow!("could not reach GitHub: {e}")),
    };

    let body = response
        .into_body()
        .into_with_config()
        .limit(MAX_METADATA_BYTES)
        .read_to_string()
        .context("could not read GitHub's answer")?;

    let release: ReleaseJson =
        serde_json::from_str(&body).context("GitHub's answer was not the release JSON")?;
    newer_than(&release, &current_version())
}

/// The decision half of [`check`], with the network taken out so it can be
/// tested against a recorded payload.
fn newer_than(release: &ReleaseJson, current: &Version) -> Result<Option<Available>> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = Version::parse(release.tag_name.trim_start_matches('v'))
        .with_context(|| format!("release tag {:?} is not a version", release.tag_name))?;
    if version <= *current {
        return Ok(None);
    }

    let Some(name) = asset_name() else {
        bail!(
            "there is no mcpurge build for {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };
    let signature_name = format!("{name}{SIGNATURE_SUFFIX}");

    let binary = release
        .assets
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| anyhow!("release {} has no {name}", release.tag_name))?;
    let signature = release
        .assets
        .iter()
        .find(|a| a.name == signature_name)
        // An unsigned release is not a release, as far as this is concerned.
        .ok_or_else(|| anyhow!("release {} has no signature for {name}", release.tag_name))?;

    Ok(Some(Available {
        version,
        tag: release.tag_name.clone(),
        notes: release.body.clone().unwrap_or_default(),
        binary_url: binary.browser_download_url.clone(),
        signature_url: signature.browser_download_url.clone(),
        size_bytes: binary.size,
    }))
}

/// Fetch the binary and its signature, and return the bytes only if the
/// signature checks out.
///
/// Verification happens here rather than in the caller so that there is no way
/// to end up holding unverified bytes and a path to write them to.
pub fn download_verified(available: &Available) -> Result<Vec<u8>> {
    let agent = agent();

    let signature = agent
        .get(&available.signature_url)
        .call()
        .context("could not download the signature")?
        .body_mut()
        .with_config()
        .limit(MAX_SIGNATURE_BYTES)
        .read_to_string()
        .context("could not read the signature")?;

    let mut binary = Vec::with_capacity(available.size_bytes.min(MAX_BINARY_BYTES) as usize);
    agent
        .get(&available.binary_url)
        .call()
        .context("could not download the new version")?
        .body_mut()
        .with_config()
        .limit(MAX_BINARY_BYTES)
        .reader()
        .read_to_end(&mut binary)
        .context("could not read the download")?;

    verify(&binary, &signature)?;
    Ok(binary)
}

/// Check a detached minisign signature against the compiled-in release key.
///
/// Three things have to hold, and the library checks all three: the signature
/// was made by the key whose ID is in [`SIGNING_KEY`], the Ed25519 signature
/// over the file's BLAKE2b hash is good, and a second Ed25519 signature covers
/// the first one together with the trusted comment - so the comment cannot be
/// rewritten either.
///
/// Legacy mode is refused (`allow_legacy: false`). minisign signed the file
/// itself rather than a hash before version 0.8; nothing mcpurge publishes is
/// in that format, and accepting it would mean a second code path through the
/// verifier for no gain.
pub fn verify(binary: &[u8], signature: &str) -> Result<()> {
    let key = PublicKey::decode(SIGNING_KEY)
        .map_err(|e| anyhow!("the built-in release signing key did not parse: {e}"))?;
    let signature = Signature::decode(signature)
        .map_err(|e| anyhow!("the downloaded signature did not parse: {e}"))?;

    // Each refusal says which of the three checks failed, because "signature
    // invalid" on a download is the moment a user most needs to know whether
    // they are looking at a broken mirror or a forgery.
    key.verify(binary, &signature, false).map_err(|e| match e {
        minisign_verify::Error::UnexpectedKeyId => anyhow!(
            "the download is signed by a different key than the mcpurge release key \
             ({SIGNING_KEY_ID})"
        ),
        minisign_verify::Error::UnexpectedAlgorithm => anyhow!(
            "the signature is in minisign's legacy format, and mcpurge releases are \
             signed in the pre-hashed one"
        ),
        e => anyhow!("the download is not signed by the mcpurge release key: {e}"),
    })
}

/// Put verified bytes in place of the running executable.
///
/// The new binary is written beside the old one first, because a replacement
/// is a rename and a rename only works within one filesystem.
pub fn install(binary: &[u8]) -> Result<PathBuf> {
    let location = Location::of_current_exe()?;
    let exe = match &location {
        Location::CargoTarget(p) => bail!(
            "{} is a cargo build directory, not an install. Updating it would overwrite \
             your build output, so mcpurge will not do it; install the new version where \
             you keep it instead.",
            p.display()
        ),
        Location::Installed(p) => p.clone(),
    };

    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", exe.display()))?;
    let staged = dir.join(format!(
        ".mcpurge-update-{}{}",
        std::process::id(),
        if cfg!(windows) { ".exe" } else { "" }
    ));

    // Written, flushed and closed before anything touches the real binary, so
    // a failed or short write cannot become a half-installed executable.
    std::fs::write(&staged, binary)
        .with_context(|| format!("could not write {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("could not make {} executable", staged.display()))?;
    }

    if let Err(e) = self_replace::self_replace(&staged) {
        // Leaving a stray file behind would be its own small bug.
        let _ = std::fs::remove_file(&staged);
        return Err(anyhow!("could not replace {}: {e}", exe.display()));
    }
    let _ = std::fs::remove_file(&staged);
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signed with the real release key, so this also proves the key shipped
    /// in the repo is the key that signs things. Regenerate every fixture here
    /// with `scripts/sign-release-fixture.sh` if the key is ever rotated.
    const FIXTURE: &str = include_str!("../tests/fixtures/signed-message.txt");
    const FIXTURE_SIG: &str = include_str!("../tests/fixtures/signed-message.txt.minisig");
    /// The same file, signed by a key that is not the release key.
    const FIXTURE_SIG_WRONG_KEY: &str =
        include_str!("../tests/fixtures/signed-message.txt.wrong-key.minisig");
    /// The same file, signed by the release key in minisign's legacy format.
    const FIXTURE_SIG_LEGACY: &str =
        include_str!("../tests/fixtures/signed-message.txt.legacy.minisig");
    const RELEASE_JSON: &str = include_str!("../tests/fixtures/release-latest.json");

    /// A minisign signature file is four lines: a comment, the signature, the
    /// trusted comment, and the signature over those last two together.
    const SIGNATURE_LINE: usize = 1;
    const TRUSTED_COMMENT_LINE: usize = 2;
    const GLOBAL_SIGNATURE_LINE: usize = 3;

    /// Alter one line of a signature file, leaving the rest intact.
    fn with_line(signature: &str, index: usize, replacement: &str) -> String {
        let mut lines: Vec<&str> = signature.lines().collect();
        assert_eq!(lines.len(), 4, "not a minisign signature: {signature:?}");
        lines[index] = replacement;
        lines.join("\n")
    }

    /// Change one letter of a base64 line, which is enough to break whichever
    /// signature it holds without making the file unparseable.
    fn one_letter_changed(line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut changed = false;
        for c in line.chars() {
            if !changed && c.is_ascii_alphabetic() {
                out.push(if c == 'A' { 'B' } else { 'A' });
                changed = true;
            } else {
                out.push(c);
            }
        }
        assert!(changed, "no letter to change in {line:?}");
        out
    }

    /// Not run by default: it talks to GitHub. Run it by hand with
    /// `cargo test -- --ignored` to prove the whole path works - TLS through
    /// the platform's own stack, the API shape, and the version comparison.
    #[test]
    #[ignore = "hits the network"]
    fn the_release_api_answers() {
        match check() {
            Ok(Some(available)) => println!("update available: {available}"),
            Ok(None) => println!("no newer release (or none published yet)"),
            Err(e) => panic!("the release check failed: {e:#}"),
        }
    }

    #[test]
    fn the_crate_version_is_a_version() {
        assert_ne!(current_version(), Version::new(0, 0, 0));
    }

    #[test]
    fn this_platform_has_a_release_asset() {
        // The three platforms CI builds for. A fourth would need adding in two
        // places, and this is the one that notices.
        assert!(asset_name().is_some(), "no asset name for this platform");
    }

    /// A tripwire for the key file being replaced: minisign writes the key ID
    /// into its comment line, so a different key is a different comment. It is
    /// the cheap half of the check - `a_real_signature_verifies` is the half
    /// that cannot be satisfied by editing text.
    #[test]
    fn the_built_in_key_is_the_release_key() {
        PublicKey::decode(SIGNING_KEY).expect("the shipped key parses");
        let comment = SIGNING_KEY.lines().next().unwrap_or_default();
        assert!(
            comment.contains(SIGNING_KEY_ID),
            "the shipped public key says {comment:?}, not {SIGNING_KEY_ID}"
        );
    }

    /// The one that ties the repo to the key: these bytes were signed by the
    /// private half, so this passing means the committed public half is its
    /// pair. It is also what proves releases are signed in the pre-hashed
    /// format, since `verify` refuses any other.
    #[test]
    fn a_real_signature_verifies() {
        verify(FIXTURE.as_bytes(), FIXTURE_SIG).expect("the fixture is signed by the release key");
    }

    /// The point of the whole module: altered bytes must not pass.
    #[test]
    fn a_tampered_download_is_refused() {
        let mut tampered = FIXTURE.as_bytes().to_vec();
        tampered.push(b'!');
        let err = verify(&tampered, FIXTURE_SIG).expect_err("tampered data must not verify");
        assert!(
            err.to_string()
                .contains("not signed by the mcpurge release key"),
            "unhelpful error: {err}"
        );
    }

    /// A real signature over these exact bytes, made by a real Ed25519 key
    /// that is not ours. Nothing about the file is malformed, so this is the
    /// test that the *key* is what is being checked.
    #[test]
    fn a_signature_from_another_key_is_refused() {
        let err = verify(FIXTURE.as_bytes(), FIXTURE_SIG_WRONG_KEY)
            .expect_err("another key's signature must not verify");
        assert!(
            err.to_string().contains("different key"),
            "unhelpful error: {err}"
        );
    }

    /// Made by the release key, and still refused, because `verify` does not
    /// accept the legacy format. Without this fixture the `allow_legacy: false`
    /// argument would be untested.
    #[test]
    fn a_legacy_signature_is_refused() {
        let err = verify(FIXTURE.as_bytes(), FIXTURE_SIG_LEGACY)
            .expect_err("a legacy signature must not verify");
        assert!(
            err.to_string().contains("legacy format"),
            "unhelpful error: {err}"
        );
    }

    /// The trusted comment is the one piece of a signature a tool will print
    /// back to a user, so it is covered by a signature of its own. Editing it
    /// has to break the file.
    #[test]
    fn an_edited_trusted_comment_is_refused() {
        let edited = with_line(
            FIXTURE_SIG,
            TRUSTED_COMMENT_LINE,
            "trusted comment: mcpurge 99.0.0, definitely genuine",
        );
        assert!(verify(FIXTURE.as_bytes(), &edited).is_err());
    }

    /// Both signatures in the file, each refused on its own: the first covers
    /// the download, the second covers the first.
    #[test]
    fn a_corrupted_signature_is_refused() {
        for line in [SIGNATURE_LINE, GLOBAL_SIGNATURE_LINE] {
            let original = FIXTURE_SIG.lines().nth(line).expect("the line is there");
            let corrupted = with_line(FIXTURE_SIG, line, &one_letter_changed(original));
            assert_ne!(corrupted, FIXTURE_SIG, "line {line} was not altered");
            assert!(
                verify(FIXTURE.as_bytes(), &corrupted).is_err(),
                "a corrupted line {line} verified"
            );
        }
    }

    fn release() -> ReleaseJson {
        serde_json::from_str(RELEASE_JSON).expect("the recorded payload parses")
    }

    #[test]
    fn a_newer_release_is_offered() {
        let found = newer_than(&release(), &Version::new(0, 1, 0))
            .expect("the payload is usable")
            .expect("0.2.0 is newer than 0.1.0");
        assert_eq!(found.version, Version::new(0, 2, 0));
        assert_eq!(found.tag, "v0.2.0");
        assert!(found.binary_url.ends_with(asset_name().unwrap()));
        assert_eq!(
            found.signature_url,
            format!("{}{SIGNATURE_SUFFIX}", found.binary_url)
        );
    }

    #[test]
    fn the_same_or_a_newer_build_is_left_alone() {
        for current in [Version::new(0, 2, 0), Version::new(0, 3, 0)] {
            assert_eq!(
                newer_than(&release(), &current).expect("usable"),
                None,
                "{current} should not be offered an update"
            );
        }
    }

    #[test]
    fn drafts_and_prereleases_are_ignored() {
        let mut r = release();
        r.draft = true;
        assert_eq!(newer_than(&r, &Version::new(0, 1, 0)).unwrap(), None);

        let mut r = release();
        r.prerelease = true;
        assert_eq!(newer_than(&r, &Version::new(0, 1, 0)).unwrap(), None);
    }

    /// A release whose signature is missing is not a release to install.
    #[test]
    fn an_unsigned_release_is_an_error() {
        let mut r = release();
        r.assets.retain(|a| !a.name.ends_with(SIGNATURE_SUFFIX));
        let err = newer_than(&r, &Version::new(0, 1, 0)).expect_err("must not be usable");
        assert!(err.to_string().contains("no signature"), "got: {err}");
    }

    #[test]
    fn a_build_directory_is_not_an_install() {
        let root = std::env::temp_dir().join(format!("mcpurge-loc-{}", std::process::id()));
        let target = root.join("target").join("release");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]").unwrap();
        let exe = target.join("mcpurge");

        assert!(matches!(Location::of(&exe), Location::CargoTarget(_)));
        // The same path without a manifest beside `target` is just a directory
        // that happens to be called target.
        std::fs::remove_file(root.join("Cargo.toml")).unwrap();
        assert!(matches!(Location::of(&exe), Location::Installed(_)));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_ordinary_path_is_an_install() {
        assert!(matches!(
            Location::of(Path::new("/usr/local/bin/mcpurge")),
            Location::Installed(_)
        ));
    }
}
