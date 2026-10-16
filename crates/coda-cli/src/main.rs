// SPDX-License-Identifier: AGPL-3.0-only

const BIN_NAME: &str = "coda";

fn version_line() -> String {
    format!("{BIN_NAME} {}", env!("CARGO_PKG_VERSION"))
}

fn main() {
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("{}", version_line());
    }
}

#[cfg(test)]
mod tests {
    use super::version_line;

    #[test]
    fn version_line_is_name_then_package_version() {
        // Arrange
        let expected = concat!("coda ", env!("CARGO_PKG_VERSION"));

        // Act
        let line = version_line();

        // Assert
        assert_eq!(line, expected);
    }
}
