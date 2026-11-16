//! RFC 3339 timestamps, as the daemon writes them.

pub fn rfc3339() -> String {
    rfc3339_at(tm_token::now())
}

/// `secs` since the Unix epoch as UTC `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339_at(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", tod / 3600, (tod % 3600) / 60, tod % 60)
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_a_known_instant() {
        assert_eq!(super::rfc3339_at(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
