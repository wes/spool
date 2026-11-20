//! Finding, checking and installing a newer Spool.
//!
//! Spool is distributed as a signed, notarized disk image, attached to a
//! release on its own repository — see `RELEASING.md`. That is the whole supply chain, and it
//! is also the whole trust story: the only thing this module will install is a
//! bundle Apple has notarized and whose leaf certificate carries the same team
//! identifier as the copy that is running. Anything else is a download that
//! failed, however well-formed its JSON was.
//!
//! There is no Sparkle here, and no framework of any kind. The four steps an
//! updater needs — ask what the latest version is, fetch it, prove it is ours,
//! put it in place — are each small, and each one is a place where a
//! general-purpose updater would have made a decision on our behalf that we
//! would then have had to check anyway.
//!
//! The order matters and is the same order as the functions below. Nothing
//! touches the installed bundle until [`verify`] has returned, and [`verify`]
//! is the only thing standing between a compromised release host and a signed
//! app being replaced by someone else's.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Where releases are published: the source repository's own releases.
///
/// Up to 0.1.15 they went to `wes/spool-releases` instead, and every copy of
/// those versions still asks there. Those copies trust only the team that
/// signed them, not [`TEAM_ID`], so there is nothing they could install from
/// here: that repository stays archived at 0.1.15, and they update by hand.
const RELEASES_API: &str =
    "https://api.github.com/repos/wes/spool/releases/latest";

/// The team the running copy was signed by, and the only team whose builds
/// will be installed over it.
///
/// Hard-coded rather than read from the running bundle. Reading it would mean
/// an attacker who could replace the app once could then nominate their own
/// team for every update after — the check would pass while meaning nothing.
///
/// From 0.1.16. Up to 0.1.15 Spool was signed by an individual team,
/// `288BJX6YHP`, and those copies pin that team instead; a build from this one
/// is, to them, signed by someone else. Changing this again strands every copy
/// that has it in the same way.
const TEAM_ID: &str = "ULMVR3KWCR";

/// A ceiling on the disk image, so a hostile or broken host cannot stream
/// unbounded bytes into memory. The real thing is around 30 MB.
const MAX_DMG_BYTES: usize = 250 * 1024 * 1024;

/// A ceiling on the release metadata, which is a few kilobytes of JSON.
const MAX_JSON_BYTES: usize = 1024 * 1024;

/// A three-part version, which is all the tags this project uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// Reads `0.2.0` or `v0.2.0`, ignoring anything after the patch number so
    /// a `-beta` suffix parses as the release it is a candidate for.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let raw = raw.strip_prefix(['v', 'V']).unwrap_or(raw);
        // A pre-release or build suffix is not ordered here: this compares
        // releases, and the only tags that reach the public repo are releases.
        let raw = raw.split(['-', '+']).next()?;

        let mut parts = raw.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        // `1.2.3.4` is not a version this project produces, and quietly
        // reading it as 1.2.3 would be a guess about which part was dropped.
        if parts.next().is_some() {
            return None;
        }
        Some(Self(major, minor, patch))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The version this build is.
pub fn current() -> Version {
    // Compiled in from `Cargo.toml`, which `RELEASING.md` requires to agree
    // with the tag and with `Info.plist`.
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version(0, 0, 0))
}

/// A release newer than this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The changelog section for it, as the release notes.
    pub notes: String,
    pub dmg_url: String,
    /// Where to send someone whose copy cannot update itself.
    pub page_url: String,
}

/// Asks what the latest release is, and returns it only if it is newer.
///
/// `Ok(None)` is the ordinary answer: it means this copy is current.
pub async fn check() -> Result<Option<Release>> {
    let (status, body) = spool_net::http::fetch_status(
        RELEASES_API,
        "application/vnd.github+json",
        MAX_JSON_BYTES,
    )
    .await
    .context("asking for the latest release")?;

    // No releases published yet. GitHub answers `releases/latest` with a 404
    // when a repository has none, which is the same answer it gives for a
    // repository that is not there — and from here the two mean the same
    // thing: there is nothing to update to.
    if status == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&status) {
        bail!("the release server returned HTTP {status}");
    }

    let json: serde_json::Value =
        serde_json::from_slice(&body).context("reading the release list")?;

    latest_from(&json)
}

/// The half of [`check`] that does not need the network, so it can be tested.
fn latest_from(json: &serde_json::Value) -> Result<Option<Release>> {
    // A draft is not published and a prerelease is not for everyone; either
    // one arriving here means the release workflow did something unusual, and
    // installing it would be worse than doing nothing.
    if json["draft"].as_bool() == Some(true) || json["prerelease"].as_bool() == Some(true) {
        return Ok(None);
    }

    let tag = json["tag_name"].as_str().context("the release has no tag")?;
    let version = Version::parse(tag)
        .with_context(|| format!("unreadable release tag: {tag:?}"))?;

    if version <= current() {
        return Ok(None);
    }

    let dmg_url = json["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|asset| asset["browser_download_url"].as_str())
        .find(|url| url.to_ascii_lowercase().ends_with(".dmg"))
        .with_context(|| format!("release {tag} has no disk image attached"))?
        .to_string();

    Ok(Some(Release {
        version,
        notes: json["body"].as_str().unwrap_or_default().trim().to_string(),
        dmg_url,
        page_url: json["html_url"].as_str().unwrap_or_default().to_string(),
    }))
}

/// Downloads the disk image to a file of our own.
///
/// Written under the system temporary directory rather than beside the app:
/// nothing here is installed yet, and a half-downloaded disk image sitting in
/// `/Applications` is litter that outlives the attempt that made it.
pub async fn download(release: &Release) -> Result<PathBuf> {
    let bytes = spool_net::http::fetch(&release.dmg_url, "application/octet-stream", MAX_DMG_BYTES)
        .await
        .with_context(|| format!("downloading Spool {}", release.version))?;

    let path = std::env::temp_dir()
        .join(format!("spool-update-{}-{}.dmg", release.version, std::process::id()));
    std::fs::write(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// A mounted disk image, which unmounts itself when it goes out of scope.
///
/// A `Drop` rather than a call at the end of [`verify`], because every early
/// return in there is a failure — exactly the cases where a volume left
/// mounted is most likely, and least likely to be noticed.
pub struct Mounted {
    mountpoint: PathBuf,
}

impl Mounted {
    pub fn app(&self) -> PathBuf {
        self.mountpoint.join("Spool.app")
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        // Best effort, and quiet: this runs while an error is on its way up,
        // and a detach that fails is not the thing worth reporting.
        Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&self.mountpoint)
            .status()
            .ok();
    }
}

/// Mounts the disk image and proves that what is on it is ours.
///
/// Three separate questions, and all three have to be answered yes:
///
/// 1. Is the signature intact — has any byte of the bundle changed since it
///    was signed? That is `codesign --verify --deep --strict`.
/// 2. Was it signed by *us*? A valid Developer ID signature only says that
///    somebody with a Developer ID made it, and anyone can get one. The
///    requirement string pins the leaf certificate to this team.
/// 3. Has Apple notarized it? `spctl` is what Gatekeeper itself would ask on
///    first launch, and asking now means a rejection is an update that did
///    not happen rather than an app that will not open.
pub fn verify(dmg: &Path) -> Result<Mounted> {
    let mountpoint = std::env::temp_dir()
        .join(format!("spool-update-mount-{}", std::process::id()));
    // A stale mountpoint from a previous attempt would make `hdiutil` refuse.
    std::fs::remove_dir_all(&mountpoint).ok();

    let attached = Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-noverify", "-mountpoint"])
        .arg(&mountpoint)
        .arg(dmg)
        .output()
        .context("running hdiutil")?;
    if !attached.status.success() {
        bail!("the disk image would not mount: {}", trimmed(&attached.stderr));
    }

    let mounted = Mounted { mountpoint };
    let app = mounted.app();
    if !app.join("Contents/Info.plist").is_file() {
        bail!("the disk image does not contain Spool.app");
    }

    run(
        "/usr/bin/codesign",
        &["--verify", "--deep", "--strict"],
        &app,
        "the downloaded copy is not intact",
    )?;

    run(
        "/usr/bin/codesign",
        &["--verify", "-R", &requirement()],
        &app,
        "the downloaded copy was signed by someone else",
    )?;

    run(
        "/usr/sbin/spctl",
        &["--assess", "--type", "execute"],
        &app,
        "the downloaded copy has not been notarized",
    )?;

    Ok(mounted)
}

/// The code requirement that pins a bundle to this team.
///
/// The team identifier is quoted because the requirement language reads an
/// unquoted one that starts with a digit — `288BJX6YHP`, the team before
/// this one — as a number followed by junk, and rejects the whole
/// string as a syntax error — which `codesign` reports exactly as it reports
/// a bundle signed by someone else.
fn requirement() -> String {
    format!("=anchor apple generic and certificate leaf[subject.OU] = \"{TEAM_ID}\"")
}

/// Replaces the running bundle with the verified one and starts it.
///
/// The swap is a single `renamex_np(RENAME_SWAP)`: the two directories trade
/// places in one step, so there is no moment when Spool is not installed. A
/// copy-over-the-top would leave a bundle that was half one version and half
/// the other if anything went wrong in the middle, and that bundle would fail
/// its own signature check for ever after.
///
/// The caller is expected to quit immediately afterwards. Replacing a running
/// app is allowed — the old executable stays alive through its open file
/// handle — but the copy on disk is no longer the one in memory, and the
/// longer that is true the stranger the process gets.
pub fn install(mounted: &Mounted) -> Result<()> {
    let installed = installed_bundle()?;
    let parent = installed.parent().context("the app is installed at the root")?;

    // Staged beside the app rather than in the temporary directory, because
    // the swap below needs both paths on one filesystem, and `/tmp` on macOS
    // is frequently not the volume `/Applications` is on.
    let staged = parent.join(format!(".spool-update-{}.app", std::process::id()));
    std::fs::remove_dir_all(&staged).ok();

    // `ditto` rather than `fs::copy` or `cp -R`: a bundle is a tree with
    // extended attributes and symlinks in it, and its signature covers them.
    // This is Apple's own tool for moving one without breaking it.
    let copied = Command::new("/usr/bin/ditto")
        .arg(mounted.app())
        .arg(&staged)
        .output()
        .context("running ditto")?;
    if !copied.status.success() {
        // A permission error here is the ordinary case — an app in
        // `/Applications` that this user does not own — and it is worth
        // saying so rather than reporting ditto's exit code.
        bail!(
            "could not write next to {}: {}",
            installed.display(),
            trimmed(&copied.stderr)
        );
    }

    // The disk image came from the internet, so everything on it carries a
    // quarantine attribute. Left in place, the relaunch below would put a
    // Gatekeeper dialog in front of an app the user has just updated on
    // purpose — and one this module has already verified more strictly than
    // that dialog would.
    Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&staged)
        .status()
        .ok();

    swap(&staged, &installed).inspect_err(|_| {
        std::fs::remove_dir_all(&staged).ok();
    })?;

    // `staged` now names the *old* bundle, the swap having exchanged them.
    std::fs::remove_dir_all(&staged).ok();

    relaunch(&installed)
}

/// Exchanges two paths atomically.
fn swap(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())?;
    let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())?;

    // SAFETY: both pointers are NUL-terminated strings that outlive the call.
    let result =
        unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_SWAP) };

    if result != 0 {
        let error = std::io::Error::last_os_error();
        bail!("could not put the new version in place: {error}");
    }
    Ok(())
}

/// Starts the newly installed copy as a separate instance.
///
/// `-n` because this process is still running for another moment; without it
/// `open` would activate the app that is already there, which is the old one,
/// and the update would look as though it had done nothing.
fn relaunch(app: &Path) -> Result<()> {
    Command::new("/usr/bin/open")
        .arg("-n")
        .arg(app)
        .status()
        .context("starting the new version")?;
    Ok(())
}

/// The `.app` this process is running out of.
///
/// `Err` when there is not one, which is the everyday case during development:
/// `cargo run` produces a bare executable in `target/`, and there is nothing
/// there an update could sensibly replace.
pub fn installed_bundle() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding the running executable")?;
    bundle_for(&exe).context("Spool is not running from an installed app bundle")
}

/// The `.app` an executable at this path belongs to, if it belongs to one.
///
/// Split from [`installed_bundle`] because `current_exe` cannot be faked, and
/// this is the arithmetic worth being sure about: three levels up, and then
/// two questions that a directory merely *named* something.app would fail.
fn bundle_for(exe: &Path) -> Option<PathBuf> {
    // Contents/MacOS/spool -> Contents/MacOS -> Contents -> Spool.app
    exe.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|path| path.extension().is_some_and(|e| e == "app"))
        .filter(|path| path.join("Contents/Info.plist").is_file())
        .map(Path::to_path_buf)
}

/// Whether this copy is one that could install an update over itself.
pub fn can_install() -> bool {
    installed_bundle().is_ok()
}

/// Runs a checking tool, turning a non-zero exit into the sentence to show.
fn run(program: &str, args: &[&str], app: &Path, failure: &str) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .arg(app)
        .output()
        .with_context(|| format!("running {program}"))?;

    if !output.status.success() {
        // These tools say what they found on stderr, and it is worth keeping:
        // "code object is not signed at all" and "rejected (the code is valid
        // but does not seem to be an app)" are different problems.
        bail!("{failure}: {}", trimmed(&output.stderr));
    }
    Ok(())
}

/// One line of a tool's output, for putting in a message.
fn trimmed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).lines().next().unwrap_or_default().trim().to_string()
}

/// How far along the update is, as the sidebar needs to say it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Asking. Only ever shown for a check someone pressed for; the one on
    /// launch says nothing until it has something to say.
    Checking,
    /// There is a newer version, and nothing has been downloaded yet.
    Found(Release),
    Downloading(Release),
    /// Downloaded, mounted and proven ours. The next press installs it.
    Ready(Release),
    Installing(Release),
    /// This copy is the newest one. Shown only after a check someone asked
    /// for — silence is the right answer to a background check that found
    /// nothing.
    UpToDate,
    Failed(String),
}

/// What the workspace holds while any of that is going on.
pub struct UpdateState {
    pub stage: Stage,
    /// The mounted image, kept alive between verifying and installing. Dropping
    /// it unmounts the volume, so it has to outlive the button that installs
    /// from it.
    pub mounted: Option<Mounted>,
    /// Whether this copy could put an update in place over itself.
    ///
    /// Decided once, when the state is made, rather than asked each time the
    /// row is drawn: it is two filesystem calls and it cannot change while
    /// the app is running.
    pub installable: bool,
}

impl UpdateState {
    pub fn new(stage: Stage) -> Self {
        Self { stage, mounted: None, installable: can_install() }
    }

    /// The line the sidebar row shows.
    pub fn label(&self) -> String {
        match &self.stage {
            Stage::Checking => "Checking for updates\u{2026}".into(),
            Stage::Found(r) => format!("Spool {} is available", r.version),
            Stage::Downloading(r) => format!("Downloading Spool {}\u{2026}", r.version),
            Stage::Ready(r) => format!("Spool {} is ready to install", r.version),
            Stage::Installing(r) => format!("Installing Spool {}\u{2026}", r.version),
            Stage::UpToDate => format!("Spool {} is the latest version", current()),
            Stage::Failed(why) => why.clone(),
        }
    }

    /// A word or two for the footer, beside the version, while something is
    /// happening. `None` when the button says it instead.
    pub fn status(&self) -> Option<&'static str> {
        match &self.stage {
            Stage::Checking => Some("Checking\u{2026}"),
            Stage::Downloading(_) => Some("Downloading\u{2026}"),
            Stage::Installing(_) => Some("Installing\u{2026}"),
            Stage::UpToDate => Some("Up to date"),
            Stage::Failed(_) => Some("Update failed"),
            Stage::Found(_) | Stage::Ready(_) => None,
        }
    }

    /// What pressing the row would do, if anything.
    pub fn action(&self) -> Option<&'static str> {
        match &self.stage {
            // A copy that cannot replace itself -- a development build out of
            // `target/`, or an app installed somewhere this user cannot write
            // -- is still worth telling about the release. It just has to send
            // them somewhere that can do something about it, rather than
            // downloading 30 MB in order to fail at the last step.
            Stage::Found(_) if !self.installable => Some("Download"),
            // One press: download, prove it is ours, install, restart.
            Stage::Found(_) => Some("Update Now"),
            Stage::Ready(_) => Some("Install and Restart"),
            // A failure is worth a second try; the usual cause is a network
            // that was not there a moment ago.
            Stage::Failed(_) => Some("Try Again"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_read_with_or_without_their_v() {
        assert_eq!(Version::parse("0.2.0"), Some(Version(0, 2, 0)));
        assert_eq!(Version::parse("v0.2.0"), Some(Version(0, 2, 0)));
        assert_eq!(Version::parse(" v1.10.3 "), Some(Version(1, 10, 3)));
        // Short forms, because a tag of `v1` is a version someone will write.
        assert_eq!(Version::parse("v1"), Some(Version(1, 0, 0)));
        assert_eq!(Version::parse("v1.2"), Some(Version(1, 2, 0)));
        // A suffix names the release it is a candidate for.
        assert_eq!(Version::parse("v0.3.0-beta.1"), Some(Version(0, 3, 0)));

        assert_eq!(Version::parse("nightly"), None);
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("1.2.3.4"), None, "not a shape this project tags");
    }

    #[test]
    fn versions_order_by_number_and_not_by_text() {
        // The reason this is not a string comparison: "0.10.0" sorts before
        // "0.9.0" alphabetically, and an updater that believed that would
        // stop offering updates at the tenth minor release.
        assert!(Version(0, 10, 0) > Version(0, 9, 0));
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
        assert!(Version(0, 2, 10) > Version(0, 2, 9));
        assert_eq!(Version(0, 1, 0), Version(0, 1, 0));
    }

    /// A release payload shaped like the one GitHub returns.
    fn release_json(tag: &str, extra: &str) -> serde_json::Value {
        serde_json::from_str(&format!(
            r#"{{
                "tag_name": "{tag}",
                "html_url": "https://github.com/wes/spool/releases/tag/{tag}",
                "body": "  Fixed a thing.  ",
                {extra}
                "assets": [
                    {{ "browser_download_url": "https://example.test/Spool-1.0.0.dmg.sha256" }},
                    {{ "browser_download_url": "https://example.test/Spool-1.0.0.dmg" }}
                ]
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn a_newer_release_is_offered_with_its_disk_image() {
        let release = latest_from(&release_json("v99.0.0", "")).unwrap().expect("newer");

        assert_eq!(release.version, Version(99, 0, 0));
        // The `.sha256` beside it ends in neither `.dmg` nor nothing: picking
        // by suffix has to mean the suffix, not merely containing it.
        assert_eq!(release.dmg_url, "https://example.test/Spool-1.0.0.dmg");
        assert_eq!(release.notes, "Fixed a thing.", "notes arrive trimmed");
    }

    #[test]
    fn the_version_already_running_is_not_an_update() {
        let same = format!("v{}", current());
        assert_eq!(latest_from(&release_json(&same, "")).unwrap(), None);
        assert_eq!(latest_from(&release_json("v0.0.1", "")).unwrap(), None, "nor an older one");
    }

    #[test]
    fn a_draft_or_a_prerelease_is_left_alone() {
        assert_eq!(latest_from(&release_json("v99.0.0", r#""draft": true,"#)).unwrap(), None);
        assert_eq!(
            latest_from(&release_json("v99.0.0", r#""prerelease": true,"#)).unwrap(),
            None
        );
    }

    #[test]
    fn a_release_with_nothing_to_download_is_an_error_not_an_update() {
        // The workflow publishing a release whose upload failed is a real
        // shape, and the wrong answer is to report an update that cannot be
        // installed when it is pressed.
        let json: serde_json::Value = serde_json::from_str(
            r#"{ "tag_name": "v99.0.0", "body": "", "assets": [] }"#,
        )
        .unwrap();
        assert!(latest_from(&json).is_err());
    }

    #[test]
    fn the_row_only_offers_something_when_there_is_something_to_press() {
        let release = Release {
            version: Version(9, 9, 9),
            notes: String::new(),
            dmg_url: "https://example.test/Spool.dmg".into(),
            page_url: "https://example.test/release".into(),
        };

        // Mid-flight states are reports, not buttons: pressing "Downloading"
        // could only mean starting a second download of the same file.
        assert_eq!(UpdateState::new(Stage::Checking).action(), None);
        assert_eq!(UpdateState::new(Stage::Downloading(release.clone())).action(), None);
        assert_eq!(UpdateState::new(Stage::Installing(release.clone())).action(), None);

        assert_eq!(UpdateState::new(Stage::Ready(release.clone())).action(), Some("Install and Restart"));
        assert_eq!(UpdateState::new(Stage::Failed("no".into())).action(), Some("Try Again"));

        // This test binary cannot replace itself, so a found release sends
        // the reader to the download page instead of fetching 30 MB to fail.
        let found = UpdateState::new(Stage::Found(release.clone()));
        assert!(!found.installable);
        assert_eq!(found.action(), Some("Download"));

        let installable = UpdateState { installable: true, ..UpdateState::new(Stage::Found(release)) };
        assert_eq!(installable.action(), Some("Update Now"));
        assert_eq!(installable.status(), None, "the button says it");
        assert_eq!(UpdateState::new(Stage::Checking).status(), Some("Checking\u{2026}"));
    }

    #[test]
    fn the_team_requirement_is_one_codesign_can_read() {
        // A requirement that does not compile fails every verification, and
        // fails it with the same exit as a genuinely foreign signature — so
        // the only symptom is that no update ever installs.
        let out = std::env::temp_dir().join(format!("spool-req-{}", std::process::id()));
        let compiled = Command::new("/usr/bin/csreq")
            .arg("-r")
            .arg(requirement())
            .arg("-b")
            .arg(&out)
            .output()
            .unwrap();
        std::fs::remove_file(&out).ok();
        assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
    }

    #[test]
    fn a_test_binary_is_not_an_installed_bundle() {
        // The guard that stops `cargo run` and `cargo test` from trying to
        // replace something in `target/`.
        assert!(installed_bundle().is_err());
        assert!(!can_install());
    }

    #[test]
    fn a_bundled_executable_finds_the_app_around_it() {
        let dir = std::env::temp_dir().join(format!("spool-bundle-{}", std::process::id()));
        let app = dir.join("Spool.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), "<plist/>").unwrap();
        let exe = app.join("Contents/MacOS/spool");

        assert_eq!(bundle_for(&exe).as_deref(), Some(app.as_path()));

        // A directory named like a bundle but without the one file that makes
        // it one. Installing over this would scatter an app across somebody's
        // folder, so the name alone is not enough to act on.
        std::fs::remove_file(app.join("Contents/Info.plist")).unwrap();
        assert_eq!(bundle_for(&exe), None, "the plist is what makes it a bundle");

        // And the everyday shape it has to reject: a bare binary in target/.
        assert_eq!(bundle_for(Path::new("/Users/x/spool/target/debug/spool")), None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
