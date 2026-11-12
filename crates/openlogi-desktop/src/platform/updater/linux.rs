use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gpui_updater::{
    DriverEvent, DriverEvents, EngineConfig, Release, StaticManifestSource, UpdateDriver,
    UpdateEngine, Verification, Version,
};

use crate::platform::installation::{InstallationSource, LinuxPackage};

/// Privileged helper shipped in release packages.
pub(crate) const UPDATE_INSTALL_HELPER: &str = "/usr/libexec/openlogi-update-install";

/// Package format required by the installation that owns OpenLogi.
pub(crate) fn package_format(source: InstallationSource) -> Option<&'static str> {
    match source {
        InstallationSource::LinuxPackage(LinuxPackage::Arch) => Some("pkg.tar.zst"),
        InstallationSource::LinuxPackage(LinuxPackage::Deb) => Some("deb"),
        InstallationSource::LinuxPackage(LinuxPackage::Rpm) => Some("rpm"),
        _ => None,
    }
}

/// Validate that the selected artifact belongs to the package manager.
pub(crate) fn matches_package(source: InstallationSource, name: &str) -> bool {
    let Some(format) = package_format(source) else {
        return false;
    };

    let expected = format!(".{format}");
    name.starts_with("openlogi-v") && name.contains("-linux-") && name.ends_with(&expected)
}

/// Expected release asset name for this machine and package source.
pub(crate) fn expected_package_name(
    source: InstallationSource,
    version: &Version,
) -> Option<String> {
    let format = package_format(source)?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some(format!("openlogi-v{version}-linux-{arch}.{format}"))
}

/// Architecture label consumed by [`StaticManifestSource`].
fn manifest_arch() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("x86_64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// Build the `pkexec` argv that production `install()` runs after download.
pub(crate) fn helper_install_command(
    package: &Path,
    signature: &Path,
    checksum: &str,
    expected_name: &str,
) -> (&'static str, Vec<String>) {
    (
        "pkexec",
        vec![
            UPDATE_INSTALL_HELPER.to_owned(),
            package.to_string_lossy().into_owned(),
            signature.to_string_lossy().into_owned(),
            checksum.to_owned(),
            expected_name.to_owned(),
        ],
    )
}

/// Why the Linux package path stopped, rendered through the locale catalog.
///
/// `Other` carries an error raised by `gpui_updater` or the OS, which the
/// macOS and Windows drivers also surface untranslated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinuxUpdateFailure {
    UnsupportedInstallation,
    UnsupportedArchitecture,
    PackageMismatch,
    StateUnavailable,
    NoUpdateSelected,
    MissingSigningKey,
    HelperMissing,
    MissingSignature,
    InvalidChecksum,
    SignatureDownload(String),
    PolkitStart(String),
    InstallFailed(String),
    Relaunch(String),
    Other(String),
}

impl LinuxUpdateFailure {
    /// The message in the active locale, shown inside `updates.update_failed_message`.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::UnsupportedInstallation => tr!("updates.linux_unsupported_installation"),
            Self::UnsupportedArchitecture => tr!("updates.linux_unsupported_architecture"),
            Self::PackageMismatch => tr!("updates.linux_package_mismatch"),
            Self::StateUnavailable => tr!("updates.linux_state_unavailable"),
            Self::NoUpdateSelected => tr!("updates.linux_no_update_selected"),
            Self::MissingSigningKey => tr!("updates.linux_missing_signing_key"),
            Self::HelperMissing => tr!("updates.linux_helper_missing"),
            Self::MissingSignature => tr!("updates.linux_missing_signature"),
            Self::InvalidChecksum => tr!("updates.linux_invalid_checksum"),
            Self::SignatureDownload(error) => {
                tr!("updates.linux_signature_download_failed", error => error)
            }
            Self::PolkitStart(error) => tr!("updates.linux_polkit_start_failed", error => error),
            Self::InstallFailed(error) => tr!("updates.linux_install_failed", error => error),
            Self::Relaunch(error) => tr!("updates.linux_relaunch_failed", error => error),
            Self::Other(error) => return error.clone(),
        }
        .to_string()
    }
}

/// Report `failure` and close the current check or install.
fn fail(events: &DriverEvents, failure: &LinuxUpdateFailure) {
    events.emit(DriverEvent::Failed(failure.message()));
    events.emit(DriverEvent::Settled);
}

/// Where a started driver is in its check/install lifecycle.
///
/// Only [`Phase::apply`] writes it. `gpui_updater` forwards every check and
/// install click without a busy guard, so this is what stops a second
/// `pkexec` install while one is running.
#[derive(Debug, Clone)]
enum Phase {
    Idle,
    /// Keeps the release a failed check falls back to.
    Checking {
        previous: Option<Release>,
    },
    Available(Release),
    /// Keeps the release a failed install falls back to.
    Installing(Release),
}

/// An event that moves [`Phase`].
#[derive(Debug)]
enum Step {
    Check,
    Checked(Option<Box<Release>>),
    CheckFailed,
    Install,
    Installed,
    InstallFailed,
}

/// Why [`Phase::apply`] rejected a step.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// A check or install is already running and will settle without help.
    Busy,
    NoRelease,
    Poisoned,
}

impl Phase {
    fn apply(&mut self, step: Step) -> Result<(), Refusal> {
        let next = match (&*self, step) {
            (Self::Idle, Step::Check) => Self::Checking { previous: None },
            (Self::Available(release), Step::Check) => Self::Checking {
                previous: Some(release.clone()),
            },
            (Self::Checking { .. }, Step::Checked(found)) => {
                found.map_or(Self::Idle, |release| Self::Available(*release))
            }
            (Self::Checking { previous }, Step::CheckFailed) => {
                previous.clone().map_or(Self::Idle, Self::Available)
            }
            (Self::Idle, Step::Install) => return Err(Refusal::NoRelease),
            (Self::Available(release), Step::Install) => Self::Installing(release.clone()),
            (Self::Installing(_), Step::Installed) => Self::Idle,
            (Self::Installing(release), Step::InstallFailed) => Self::Available(release.clone()),
            _ => return Err(Refusal::Busy),
        };
        *self = next;
        Ok(())
    }
}

/// The event sink and lifecycle phase, which `start` hands over together.
struct Session {
    events: DriverEvents,
    phase: Arc<Mutex<Phase>>,
}

impl Session {
    /// Apply `step`, reporting a refusal the user must see. Returns the new
    /// phase on success.
    fn begin(&self, step: Step) -> Option<Phase> {
        let refusal = match self.phase.lock() {
            Ok(mut phase) => match phase.apply(step) {
                Ok(()) => return Some(phase.clone()),
                Err(refusal) => refusal,
            },
            Err(_) => Refusal::Poisoned,
        };
        match refusal {
            Refusal::Busy => {}
            Refusal::NoRelease => fail(&self.events, &LinuxUpdateFailure::NoUpdateSelected),
            Refusal::Poisoned => fail(&self.events, &LinuxUpdateFailure::StateUnavailable),
        }
        None
    }
}

/// Apply a step that a worker thread reports when it finishes. Its own
/// `Checking`/`Installing` phase stays in place until then, because `begin`
/// refuses every other step while it runs.
fn finish(phase: &Mutex<Phase>, step: Step) {
    if let Ok(mut phase) = phase.lock() {
        let applied = phase.apply(step);
        debug_assert!(applied.is_ok(), "worker finished outside its phase");
    }
}

/// Update driver for Linux installations managed by a package manager.
pub(crate) struct LinuxPackageDriver {
    manifest_url: String,
    public_key: Option<String>,
    current_version: Version,
    /// `None` until [`UpdateDriver::start`].
    session: Option<Session>,
}

impl LinuxPackageDriver {
    pub(crate) fn new(
        manifest_url: String,
        public_key: Option<String>,
        current_version: Version,
    ) -> Self {
        Self {
            manifest_url,
            public_key,
            current_version,
            session: None,
        }
    }
}

impl UpdateDriver for LinuxPackageDriver {
    fn start(&mut self, events: DriverEvents) {
        self.session = Some(Session {
            events,
            phase: Arc::new(Mutex::new(Phase::Idle)),
        });
    }

    fn check(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        if session.begin(Step::Check).is_none() {
            return;
        }

        let events = session.events.clone();
        let phase = Arc::clone(&session.phase);
        let url = self.manifest_url.clone();
        let key = self.public_key.clone();
        let version = self.current_version.clone();

        events.emit(DriverEvent::Checking);

        std::thread::spawn(move || {
            match find_release(url, key, version) {
                Ok(found) => {
                    finish(&phase, Step::Checked(found.clone().map(Box::new)));
                    events.emit(found.map_or(DriverEvent::UpToDate, DriverEvent::Found));
                }
                Err(failure) => {
                    finish(&phase, Step::CheckFailed);
                    events.emit(DriverEvent::Failed(failure.message()));
                }
            }
            events.emit(DriverEvent::Settled);
        });
    }

    fn install(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        let Some(Phase::Installing(release)) = session.begin(Step::Install) else {
            return;
        };

        let events = session.events.clone();
        let phase = Arc::clone(&session.phase);
        let url = self.manifest_url.clone();
        let key = self.public_key.clone();
        let version = self.current_version.clone();

        std::thread::spawn(move || {
            match install_verified_package(release, url, key, version, &events) {
                Ok(version) => {
                    finish(&phase, Step::Installed);
                    events.emit(DriverEvent::ReadyToRelaunch(version));
                }
                Err(failure) => {
                    finish(&phase, Step::InstallFailed);
                    events.emit(DriverEvent::Failed(failure.message()));
                }
            }
            events.emit(DriverEvent::Settled);
        });
    }

    fn relaunch(&mut self) {
        use std::os::unix::process::CommandExt;

        let executable = "/usr/bin/openlogi-desktop";

        // exec replaces the current process with the newly installed binary.
        // It avoids opening a second instance while the old one is running.
        let error = std::process::Command::new(executable).exec();

        if let Some(session) = &self.session {
            fail(
                &session.events,
                &LinuxUpdateFailure::Relaunch(error.to_string()),
            );
        }
    }
}

/// Look up the newest release whose package matches this installation.
fn find_release(
    url: String,
    key: Option<String>,
    version: Version,
) -> Result<Option<Release>, LinuxUpdateFailure> {
    let installation = InstallationSource::detect();
    let format = package_format(installation).ok_or(LinuxUpdateFailure::UnsupportedInstallation)?;
    let arch = manifest_arch().ok_or(LinuxUpdateFailure::UnsupportedArchitecture)?;

    let source = StaticManifestSource::new(url)
        .os("linux")
        .arch(arch)
        .format(format);

    let mut config = EngineConfig::new(version).verification(Verification::Strict);
    if let Some(key) = key {
        config = config.minisign_public_key(key);
    }

    let Some(release) = UpdateEngine::new(source, config)
        .check()
        .map_err(|error| LinuxUpdateFailure::Other(error.to_string()))?
    else {
        return Ok(None);
    };

    let expected = expected_package_name(installation, &release.version)
        .ok_or(LinuxUpdateFailure::UnsupportedArchitecture)?;
    if release.asset.name != expected {
        return Err(LinuxUpdateFailure::PackageMismatch);
    }
    Ok(Some(release))
}

/// Download, verify, and elevate-install one signed Linux package release.
fn install_verified_package(
    release: Release,
    url: String,
    key: Option<String>,
    version: Version,
    events: &DriverEvents,
) -> Result<Version, LinuxUpdateFailure> {
    let installation = InstallationSource::detect();
    let Some(format) = package_format(installation) else {
        return Err(LinuxUpdateFailure::UnsupportedInstallation);
    };

    let Some(arch) = manifest_arch() else {
        return Err(LinuxUpdateFailure::UnsupportedArchitecture);
    };

    let expected_name = expected_package_name(installation, &release.version)
        .ok_or(LinuxUpdateFailure::UnsupportedArchitecture)?;

    if release.asset.name != expected_name || !matches_package(installation, &release.asset.name) {
        return Err(LinuxUpdateFailure::PackageMismatch);
    }

    let key = key.ok_or(LinuxUpdateFailure::MissingSigningKey)?;
    let temporary =
        tempfile::tempdir().map_err(|error| LinuxUpdateFailure::Other(error.to_string()))?;

    let source = StaticManifestSource::new(url)
        .os("linux")
        .arch(arch)
        .format(format);

    let config = EngineConfig::new(version)
        .verification(Verification::Strict)
        .minisign_public_key(key)
        .install_root(temporary.path().join("openlogi"))
        .download_dir(temporary.path().join("downloads"));

    let engine = UpdateEngine::new(source, config);

    events.emit(DriverEvent::Downloading {
        downloaded: 0,
        total: None,
    });

    let artifact = engine
        .download(&release, |downloaded, total| {
            events.emit(DriverEvent::Downloading { downloaded, total });
        })
        .map_err(|error| LinuxUpdateFailure::Other(error.to_string()))?;

    events.emit(DriverEvent::Installing);

    // A privileged helper is shipped with newer OpenLogi packages.
    // Older installations must be upgraded manually once.
    if !Path::new(UPDATE_INSTALL_HELPER).is_file() {
        return Err(LinuxUpdateFailure::HelperMissing);
    }

    let signature_url = release
        .signature_url
        .as_deref()
        .filter(|url| url.starts_with("https://"))
        .ok_or(LinuxUpdateFailure::MissingSignature)?;

    let checksum = release
        .sha256
        .as_deref()
        .filter(|hash| hash.len() == 64 && hash.as_bytes().iter().all(u8::is_ascii_hexdigit))
        .ok_or(LinuxUpdateFailure::InvalidChecksum)?;

    let signature: PathBuf = temporary.path().join(format!("{expected_name}.minisig"));

    let download_status = std::process::Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--output",
        ])
        .arg(&signature)
        .arg("--")
        .arg(signature_url)
        .status()
        .map_err(|error| LinuxUpdateFailure::SignatureDownload(error.to_string()))?;

    if !download_status.success() {
        return Err(LinuxUpdateFailure::SignatureDownload(
            download_status.to_string(),
        ));
    }

    // The helper copies into a root-owned directory, then verifies
    // minisign and SHA-256 before invoking the package manager.
    events.emit(DriverEvent::Installing);

    let (program, args) = helper_install_command(&artifact, &signature, checksum, &expected_name);
    let status = std::process::Command::new(program)
        .args(&args)
        .status()
        .map_err(|error| LinuxUpdateFailure::PolkitStart(error.to_string()))?;

    if !status.success() {
        return Err(LinuxUpdateFailure::InstallFailed(status.to_string()));
    }

    Ok(release.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_package_formats_for_supported_sources() {
        assert_eq!(
            package_format(InstallationSource::LinuxPackage(LinuxPackage::Arch)),
            Some("pkg.tar.zst")
        );
        assert_eq!(
            package_format(InstallationSource::LinuxPackage(LinuxPackage::Deb)),
            Some("deb")
        );
        assert_eq!(
            package_format(InstallationSource::LinuxPackage(LinuxPackage::Rpm)),
            Some("rpm")
        );
        assert_eq!(package_format(InstallationSource::Unknown), None);
    }

    #[test]
    fn rejects_wrong_package_format() {
        let arch = InstallationSource::LinuxPackage(LinuxPackage::Arch);
        let deb = InstallationSource::LinuxPackage(LinuxPackage::Deb);
        let rpm = InstallationSource::LinuxPackage(LinuxPackage::Rpm);

        assert!(matches_package(
            arch,
            "openlogi-v0.8.21-linux-amd64.pkg.tar.zst"
        ));
        assert!(matches_package(deb, "openlogi-v0.8.21-linux-amd64.deb"));
        assert!(matches_package(rpm, "openlogi-v0.8.21-linux-amd64.rpm"));

        assert!(!matches_package(arch, "openlogi-v0.8.21-linux-amd64.deb"));
        assert!(!matches_package(deb, "openlogi-v0.8.21-linux-amd64.rpm"));
        assert!(!matches_package(rpm, "untrusted.rpm"));
    }

    #[test]
    fn builds_expected_package_names() {
        let version = Version::parse("0.9.1").unwrap();
        let name = expected_package_name(
            InstallationSource::LinuxPackage(LinuxPackage::Deb),
            &version,
        )
        .unwrap();
        assert!(
            name == "openlogi-v0.9.1-linux-amd64.deb" || name == "openlogi-v0.9.1-linux-arm64.deb"
        );
    }

    #[test]
    fn failures_render_in_the_active_locale() {
        let detail = "exit status: 126";
        let failures = [
            LinuxUpdateFailure::UnsupportedInstallation,
            LinuxUpdateFailure::UnsupportedArchitecture,
            LinuxUpdateFailure::PackageMismatch,
            LinuxUpdateFailure::StateUnavailable,
            LinuxUpdateFailure::NoUpdateSelected,
            LinuxUpdateFailure::MissingSigningKey,
            LinuxUpdateFailure::HelperMissing,
            LinuxUpdateFailure::MissingSignature,
            LinuxUpdateFailure::InvalidChecksum,
            LinuxUpdateFailure::SignatureDownload(detail.to_owned()),
            LinuxUpdateFailure::PolkitStart(detail.to_owned()),
            LinuxUpdateFailure::InstallFailed(detail.to_owned()),
            LinuxUpdateFailure::Relaunch(detail.to_owned()),
        ];

        let _locale = crate::services::i18n::LOCALE_LOCK.lock().unwrap();
        rust_i18n::set_locale("en");
        let english: Vec<String> = failures.iter().map(LinuxUpdateFailure::message).collect();
        rust_i18n::set_locale("de");
        let german: Vec<String> = failures.iter().map(LinuxUpdateFailure::message).collect();
        rust_i18n::set_locale("en");

        for ((failure, en), de) in failures.iter().zip(&english).zip(&german) {
            // A missing key renders as its own path.
            assert!(
                !en.contains("updates.linux_"),
                "{failure:?} has no English entry"
            );
            assert!(
                !de.contains("updates.linux_"),
                "{failure:?} has no German entry"
            );
            assert_ne!(en, de, "{failure:?} is not translated");
            if matches!(
                failure,
                LinuxUpdateFailure::SignatureDownload(_)
                    | LinuxUpdateFailure::PolkitStart(_)
                    | LinuxUpdateFailure::InstallFailed(_)
                    | LinuxUpdateFailure::Relaunch(_)
            ) {
                assert!(de.contains(detail), "{failure:?} drops its detail: {de}");
            }
        }
    }

    #[test]
    fn engine_failures_pass_through_unchanged() {
        let failure = LinuxUpdateFailure::Other("no release asset matched".to_owned());
        assert_eq!(failure.message(), "no release asset matched");
    }

    fn release() -> Release {
        let version = Version::parse("0.9.1").unwrap();
        Release::new(
            version,
            gpui_updater::Asset::new("openlogi-v0.9.1-linux-amd64.deb", "https://x", 0),
        )
    }

    /// A started driver whose emitted events are recorded as their `Debug` form.
    fn started(phase: Phase) -> (LinuxPackageDriver, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        let mut driver = LinuxPackageDriver::new(
            "https://x".to_owned(),
            None,
            Version::parse("0.9.0").unwrap(),
        );
        driver.start(DriverEvents::new(move |event| {
            sink.lock().unwrap().push(format!("{event:?}"));
        }));
        *driver.session.as_ref().unwrap().phase.lock().unwrap() = phase;
        (driver, log)
    }

    #[test]
    fn phase_transitions() {
        use Refusal::{Busy, NoRelease};
        let checking = || Phase::Checking { previous: None };
        let cases: [(Phase, Step, Result<(), Refusal>, &str); 14] = [
            (Phase::Idle, Step::Check, Ok(()), "Checking"),
            (Phase::Available(release()), Step::Check, Ok(()), "Checking"),
            (checking(), Step::Check, Err(Busy), "Checking"),
            (
                Phase::Installing(release()),
                Step::Check,
                Err(Busy),
                "Installing",
            ),
            (
                checking(),
                Step::Checked(Some(Box::new(release()))),
                Ok(()),
                "Available",
            ),
            (checking(), Step::Checked(None), Ok(()), "Idle"),
            (checking(), Step::CheckFailed, Ok(()), "Idle"),
            (
                Phase::Checking {
                    previous: Some(release()),
                },
                Step::CheckFailed,
                Ok(()),
                "Available",
            ),
            (Phase::Idle, Step::Install, Err(NoRelease), "Idle"),
            (checking(), Step::Install, Err(Busy), "Checking"),
            (
                Phase::Available(release()),
                Step::Install,
                Ok(()),
                "Installing",
            ),
            (
                Phase::Installing(release()),
                Step::Install,
                Err(Busy),
                "Installing",
            ),
            (
                Phase::Installing(release()),
                Step::Installed,
                Ok(()),
                "Idle",
            ),
            (
                Phase::Installing(release()),
                Step::InstallFailed,
                Ok(()),
                "Available",
            ),
        ];
        for (mut phase, step, verdict, after) in cases {
            let label = format!("{phase:?} + {step:?}");
            assert_eq!(phase.apply(step), verdict, "{label}");
            assert!(
                format!("{phase:?}").starts_with(after),
                "{label} -> {phase:?}"
            );
        }
    }

    #[test]
    fn install_before_start_does_nothing() {
        let mut driver = LinuxPackageDriver::new(
            "https://x".to_owned(),
            None,
            Version::parse("0.9.0").unwrap(),
        );
        driver.install();
        assert!(driver.session.is_none());
    }

    #[test]
    fn install_without_release_reports_no_update_selected() {
        let _locale = crate::services::i18n::LOCALE_LOCK.lock().unwrap();
        rust_i18n::set_locale("en");
        let (mut driver, log) = started(Phase::Idle);
        driver.install();
        let expected = format!(
            "Failed({:?})",
            LinuxUpdateFailure::NoUpdateSelected.message()
        );
        assert_eq!(*log.lock().unwrap(), [expected, "Settled".to_owned()]);
    }

    #[test]
    fn second_install_and_check_while_installing_are_ignored() {
        let (mut driver, log) = started(Phase::Installing(release()));
        driver.install();
        driver.check();
        assert!(log.lock().unwrap().is_empty());
        let phase = driver
            .session
            .as_ref()
            .unwrap()
            .phase
            .lock()
            .unwrap()
            .clone();
        assert!(matches!(phase, Phase::Installing(_)), "{phase:?}");
    }

    #[test]
    fn builds_pkexec_helper_command_without_shell_interpolation() {
        // Avoid the contiguous 0-f alphabet that fixture-report-hex-owner flags.
        let checksum = "deadbeef".repeat(8);
        let (program, args) = helper_install_command(
            Path::new("/tmp/openlogi-v0.9.1-linux-amd64.deb"),
            Path::new("/tmp/openlogi-v0.9.1-linux-amd64.deb.minisig"),
            &checksum,
            "openlogi-v0.9.1-linux-amd64.deb",
        );
        assert_eq!(program, "pkexec");
        assert_eq!(
            args,
            [
                UPDATE_INSTALL_HELPER,
                "/tmp/openlogi-v0.9.1-linux-amd64.deb",
                "/tmp/openlogi-v0.9.1-linux-amd64.deb.minisig",
                checksum.as_str(),
                "openlogi-v0.9.1-linux-amd64.deb",
            ]
        );
    }
}
