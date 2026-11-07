// SPDX-License-Identifier: Apache-2.0

//! Byte counts for people to read.

const KIB: u64 = 1024;
const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];

/// `bytes` in the largest unit that keeps the number at 1 or more, such as `1.5 KiB`.
#[expect(
    clippy::cast_precision_loss,
    reason = "a size shown to one decimal place"
)]
pub(crate) fn human(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= KIB as f64 && unit + 1 < UNITS.len() {
        value /= KIB as f64;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::human;

    #[test]
    fn sizes_are_shown_in_the_largest_whole_unit() {
        // Act
        let shown: Vec<_> = [0, 1023, 1024, 1536, 5 * 1024 * 1024].map(human).into();

        // Assert
        assert_eq!(shown, ["0 B", "1023 B", "1.0 KiB", "1.5 KiB", "5.0 MiB"]);
    }
}
