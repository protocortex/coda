// SPDX-License-Identifier: AGPL-3.0-only

//! The installer against a local fake release, through its test-only file:// override.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const VERSION: &str = "9.9.9";
const TARGET: &str = "x86_64-unknown-linux-gnu";

struct Fixture {
    root: TempDir,
}

impl Fixture {
    /// A release directory holding a fake archive and the SHA256SUMS for it.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let name = format!("coda-{VERSION}-{TARGET}");
        let pkg = root.path().join("pkg").join(&name);
        fs::create_dir_all(&pkg).unwrap();
        let bin = pkg.join("coda");
        fs::write(&bin, "#!/bin/sh\necho 'coda 9.9.9'\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();

        let release = root.path().join("release");
        fs::create_dir_all(&release).unwrap();
        let archive = format!("{name}.tar.gz");
        run(Command::new("tar")
            .args(["-czf"])
            .arg(release.join(&archive))
            .arg("-C")
            .arg(root.path().join("pkg"))
            .arg(&name));

        let sum = checksum(&release.join(&archive));
        fs::write(release.join("SHA256SUMS"), format!("{sum}  {archive}\n")).unwrap();

        // A uname that reports a supported platform on any host.
        let fakebin = root.path().join("fakebin");
        fs::create_dir_all(&fakebin).unwrap();
        write_uname(&fakebin, "Linux", "x86_64");
        Self { root }
    }

    fn release(&self) -> PathBuf {
        self.root.path().join("release")
    }

    fn install_dir(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    fn fakebin(&self) -> PathBuf {
        self.root.path().join("fakebin")
    }

    fn install(&self) -> Output {
        let path = format!(
            "{}:{}",
            self.fakebin().display(),
            std::env::var("PATH").unwrap()
        );
        Command::new("sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh"))
            .env("PATH", path)
            .env("CODA_VERSION", VERSION)
            .env("CODA_INSTALL_DIR", self.install_dir())
            .env("CODA_INSTALL_TEST", "1")
            .env(
                "CODA_RELEASE_BASE",
                format!("file://{}", self.release().display()),
            )
            .output()
            .expect("sh should start")
    }
}

fn run(command: &mut Command) {
    let status = command.status().expect("command should start");
    assert!(status.success(), "{command:?} failed");
}

fn write_uname(dir: &Path, os: &str, arch: &str) {
    let path = dir.join("uname");
    let script = format!("#!/bin/sh\ncase \"$1\" in -s) echo {os} ;; -m) echo {arch} ;; esac\n");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn checksum(path: &Path) -> String {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .or_else(|_| {
            Command::new("shasum")
                .args(["-a", "256"])
                .arg(path)
                .output()
        })
        .expect("sha256sum or shasum should exist");
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_good_archive_installs_an_executable_coda() {
    // Arrange
    let fixture = Fixture::new();

    // Act
    let output = fixture.install();

    // Assert
    assert!(output.status.success(), "{}", stderr(&output));
    let installed = fixture.install_dir().join("coda");
    let mode = fs::metadata(&installed).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(String::from_utf8_lossy(&output.stdout).contains("coda 9.9.9"));
}

#[test]
fn a_checksum_mismatch_fails_and_installs_nothing() {
    // Arrange
    let fixture = Fixture::new();
    let bad = format!("{}  coda-{VERSION}-{TARGET}.tar.gz\n", "0".repeat(64));
    fs::write(fixture.release().join("SHA256SUMS"), bad).unwrap();

    // Act
    let output = fixture.install();

    // Assert
    assert!(!output.status.success());
    assert!(stderr(&output).contains("checksum mismatch"));
    assert!(!fixture.install_dir().join("coda").exists());
}

#[test]
fn a_longer_name_in_sha256sums_is_not_an_entry_for_the_archive() {
    // Arrange
    let fixture = Fixture::new();
    let sum = "0".repeat(64);
    let longer = format!("{sum}  coda-{VERSION}-{TARGET}.tar.gz.bak\n");
    fs::write(fixture.release().join("SHA256SUMS"), longer).unwrap();

    // Act
    let output = fixture.install();

    // Assert
    assert!(!output.status.success());
    assert!(stderr(&output).contains("has no entry"));
    assert!(!fixture.install_dir().join("coda").exists());
}

#[test]
fn an_unsupported_platform_fails_clearly() {
    // Arrange
    let fixture = Fixture::new();
    write_uname(&fixture.fakebin(), "Plan9", "mips");

    // Act
    let output = fixture.install();

    // Assert
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unsupported platform Plan9 mips"));
    assert!(!fixture.install_dir().exists());
}

#[test]
fn an_existing_binary_is_replaced_and_no_temp_file_is_left() {
    // Arrange
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.install_dir()).unwrap();
    fs::write(fixture.install_dir().join("coda"), "old").unwrap();

    // Act
    let output = fixture.install();

    // Assert
    assert!(output.status.success(), "{}", stderr(&output));
    let installed = fs::read_to_string(fixture.install_dir().join("coda")).unwrap();
    assert!(installed.contains("coda 9.9.9"));
    let entries = fs::read_dir(fixture.install_dir()).unwrap().count();
    assert_eq!(entries, 1);
}

#[test]
fn the_release_override_is_refused_without_the_test_flag() {
    // Arrange
    let fixture = Fixture::new();

    // Act
    let output = Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh"))
        .env("CODA_VERSION", VERSION)
        .env("CODA_INSTALL_DIR", fixture.install_dir())
        .env("CODA_RELEASE_BASE", "file:///nonexistent")
        .env_remove("CODA_INSTALL_TEST")
        .output()
        .expect("sh should start");

    // Assert
    assert!(!output.status.success());
    assert!(stderr(&output).contains("only for tests"));
}
