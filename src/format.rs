//! Human-readable text for the UI: sizes, speeds, durations, times.

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

pub fn rate(bps: f64) -> String {
    if bps < 1.0 { String::new() } else { format!("{}/s", bytes(bps as u64)) }
}

/// "about 1 min 20 s left"; only for ETAs that mean something.
pub fn eta(secs: u32) -> String {
    match secs {
        0 => String::new(),
        1..=59 => format!("{secs} s left"),
        60..=3599 => format!("{} min {} s left", secs / 60, secs % 60),
        _ => format!("{} h {} min left", secs / 3600, secs % 3600 / 60),
    }
}

pub fn ago(now: u64, then: Option<u64>) -> String {
    let Some(t) = then else { return "never".into() };
    let d = now.saturating_sub(t);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86_399 => format!("{} h ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_decimal_units_with_sensible_precision() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_500), "1.50 KB");
        assert_eq!(bytes(12_345_678), "12.3 MB");
        assert_eq!(bytes(345_000_000), "345 MB");
        assert_eq!(bytes(5_000_000_000), "5.00 GB");
    }

    #[test]
    fn rates_etas_and_ages() {
        assert_eq!(rate(0.0), "");
        assert_eq!(rate(2_500_000.0), "2.50 MB/s");
        assert_eq!(eta(0), "");
        assert_eq!(eta(45), "45 s left");
        assert_eq!(eta(80), "1 min 20 s left");
        assert_eq!(eta(7_500), "2 h 5 min left");
        assert_eq!(ago(1000, None), "never");
        assert_eq!(ago(1000, Some(990)), "just now");
        assert_eq!(ago(1000, Some(1000 - 300)), "5 min ago");
        assert_eq!(ago(100_000, Some(100_000 - 7_300)), "2 h ago");
        assert_eq!(ago(1_000_000, Some(1_000_000 - 200_000)), "2 days ago");
        assert_eq!(plural(1, "file", "files"), "1 file");
        assert_eq!(plural(3, "file", "files"), "3 files");
    }
}
