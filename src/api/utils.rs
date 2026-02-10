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
