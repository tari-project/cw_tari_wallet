/// Format microTari to human-readable string (e.g., "1,234.567890 XTM").
pub fn format_micro_tari(micro_tari: u64) -> String {
    let whole_tari = micro_tari / 1_000_000;
    let fractional = micro_tari % 1_000_000;

    if whole_tari >= 1000 {
        let formatted_whole = format_with_thousands_separator(whole_tari);
        format!("{}.{:06} XTM", formatted_whole, fractional)
    } else {
        format!("{}.{:06} XTM", whole_tari, fractional)
    }
}

fn format_with_thousands_separator(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);

    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }

    result.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    //! Pure-formatting unit tests. No I/O, no global state — deterministic.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn sub_one_xtm_is_zero_whole_with_padded_fraction() {
        assert_eq!(format_micro_tari(1), "0.000001 XTM");
        assert_eq!(format_micro_tari(0), "0.000000 XTM");
    }

    #[test]
    fn exact_one_xtm() {
        assert_eq!(format_micro_tari(1_000_000), "1.000000 XTM");
    }

    #[test]
    fn fractional_zero_padding() {
        assert_eq!(format_micro_tari(1_500_000), "1.500000 XTM");
    }

    #[test]
    fn no_separator_just_below_one_thousand_xtm() {
        // 999 XTM is the largest whole value that must NOT get a separator
        // (separator only applies when whole_tari >= 1000).
        assert_eq!(format_micro_tari(999_000_000), "999.000000 XTM");
        assert_eq!(format_micro_tari(999_999_999), "999.999999 XTM");
    }

    #[test]
    fn thousands_separator_boundary() {
        // 1000 XTM is the first value that gets a thousands separator.
        assert_eq!(format_micro_tari(1_000_000_000), "1,000.000000 XTM");
    }

    #[test]
    fn large_value_with_multiple_separators() {
        // 1,234,567 XTM
        assert_eq!(format_micro_tari(1_234_567_000_000), "1,234,567.000000 XTM");
    }

    #[test]
    fn u64_max_does_not_panic_and_matches_computed_string() {
        let v = u64::MAX;
        let whole = v / 1_000_000;
        let frac = v % 1_000_000;
        let expected = format!("{}.{:06} XTM", format_with_thousands_separator(whole), frac);
        assert_eq!(format_micro_tari(v), expected);
        // Sanity-check the hand-computed value too.
        assert_eq!(format_micro_tari(v), "18,446,744,073,709.551615 XTM");
    }

    #[test]
    fn thousands_separator_groups_in_threes() {
        assert_eq!(format_with_thousands_separator(0), "0");
        assert_eq!(format_with_thousands_separator(999), "999");
        assert_eq!(format_with_thousands_separator(1_000), "1,000");
        assert_eq!(format_with_thousands_separator(1_000_000), "1,000,000");
        assert_eq!(format_with_thousands_separator(12_345), "12,345");
    }
}
