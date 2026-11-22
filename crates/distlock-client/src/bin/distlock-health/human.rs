//! Readable numbers for `check --human-readable`. Only the text output uses these; the JSON a
//! webhook receives always carries plain numbers.

/// `1234567` as `1_234_567`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(c);
    }
    out
}

/// Binary (IEC) units: `1536` as `1.5 KiB`, `17_630_003_200` as `16.4 GiB`. Under 1 KiB: `512 B`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = UNITS[0];
    for u in UNITS {
        value /= 1024.0;
        unit = u;
        // 1023.96 prints as "1024.0": move up a unit instead.
        if value < 1023.95 {
            break;
        }
    }
    format!("{value:.1} {unit}")
}

/// `24.34` as `24.3%`.
pub fn percent(p: f64) -> String {
    format!("{p:.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_get_an_underscore_every_three_digits() {
        for (n, want) in [
            (0, "0"),
            (7, "7"),
            (999, "999"),
            (1_000, "1_000"),
            (92_160, "92_160"),
            (1_234_567, "1_234_567"),
            (u64::MAX, "18_446_744_073_709_551_615"),
        ] {
            assert_eq!(count(n), want);
        }
    }

    #[test]
    fn bytes_use_binary_units_and_move_up_before_printing_1024() {
        for (n, want) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (1024 * 1024, "1.0 MiB"),
            (25_769_803_776, "24.0 GiB"),
            (1024 * 1024 - 1, "1.0 MiB"),
            (u64::MAX, "16.0 EiB"),
        ] {
            assert_eq!(bytes(n), want, "{n}");
        }
    }

    #[test]
    fn percentages_have_one_decimal() {
        assert_eq!(percent(24.34), "24.3%");
        assert_eq!(percent(0.0), "0.0%");
        assert_eq!(percent(100.0), "100.0%");
    }
}
