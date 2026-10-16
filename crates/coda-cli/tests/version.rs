// SPDX-License-Identifier: AGPL-3.0-only

use std::process::Command;

#[test]
fn version_flags_print_name_and_version_and_exit_zero() {
    for flag in ["--version", "-V"] {
        // Arrange
        let expected = format!("coda {}\n", env!("CARGO_PKG_VERSION"));

        // Act
        let output = Command::new(env!("CARGO_BIN_EXE_coda"))
            .arg(flag)
            .output()
            .expect("coda binary should start");

        // Assert
        assert!(output.status.success(), "{flag} should exit 0");
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "{flag}");
    }
}
