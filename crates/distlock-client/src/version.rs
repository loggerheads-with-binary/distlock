//! Release versions (`MAJOR.MINOR[.PATCH]`), compared as numbers.

/// The oldest server release this client works with. A client that needs something a later server
/// added (a new protocol feature, an auth mode) raises this; the user can lower or raise it per
/// connection with `min_version`.
pub(crate) const EXPECTED_SERVER_VERSION: &str = "0.5.0";

/// What a server that does not report its version is taken to be: 0.3.0, the last release before
/// servers started to say.
pub(crate) const UNREPORTING_SERVER: (u64, u64, u64) = (0, 3, 0);

/// `(major, minor, patch)` from `1.2.3`, `v1.2` or `1.2.3-rc1+build`. A pre-release or build suffix
/// is ignored, so `0.3.0-rc1` counts as `0.3.0`. `None` if it is not a version.
pub(crate) fn parse(text: &str) -> Option<(u64, u64, u64)> {
    let numeric = text.trim().trim_start_matches(['v', 'V']);
    let numeric = numeric.split(['-', '+']).next()?;
    let mut parts = numeric.split('.');
    let mut next = |required: bool| match parts.next() {
        Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            p.parse().ok().map(Some)
        }
        None if !required => Some(None),
        _ => None,
    };
    let major = next(true)??;
    let minor = next(true)??;
    let patch = next(false)?.unwrap_or(0);
    parts.next().is_none().then_some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn versions_parse_and_order_as_numbers() {
        assert_eq!(parse("0.3.0"), Some((0, 3, 0)));
        assert_eq!(parse("v1.2"), Some((1, 2, 0)));
        assert_eq!(parse(" 10.0.7-rc1+build5 "), Some((10, 0, 7)));
        assert!(
            parse("0.10.0") > parse("0.9.9"),
            "10 is more than 9, not less"
        );
        assert!(parse("1.0.0") > parse("0.99.99"));
        for bad in ["", "1", "1.x.0", "1.2.3.4", "a.b.c", "-1.0.0", "1..0"] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }
}
