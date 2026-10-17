//! Conversion between RFC 3339 timestamps and Unix seconds, without a date library.

use std::ops::Range;

/// Parses an RFC 3339 timestamp, such as `2026-10-17T07:59:59.858856+00:00`,
/// into Unix seconds. Fractional seconds are dropped.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    let separators = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];
    if bytes.len() < 20
        || separators
            .iter()
            .any(|&(i, c)| !bytes[i].eq_ignore_ascii_case(&c))
    {
        return None;
    }
    let field = |range: Range<usize>| s.get(range).and_then(number);
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = s.get(19..)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset = if rest.eq_ignore_ascii_case("z") {
        0
    } else {
        let sign = match rest.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if rest.len() != 6 || rest.as_bytes()[3] != b':' {
            return None;
        }
        sign * (rest.get(1..3).and_then(number)? * 3600 + rest.get(4..6).and_then(number)? * 60)
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_rfc3339(unix: i64) -> String {
    let (days, seconds) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// Parses a field made only of ASCII digits.
fn number(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day since 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_offsets_and_fractions() {
        let cases = [
            ("1970-01-01T00:00:00Z", 0),
            ("2026-10-17T07:59:59.858856+00:00", 1_792_223_999),
            ("2026-10-20T16:00:00+00:00", 1_792_512_000),
            ("2026-10-17T13:29:59.5-05:30", 1_792_263_599),
            ("2000-02-29T12:00:00z", 951_825_600),
            ("1969-12-31t23:59:59Z", -1),
        ];
        for (input, want) in cases {
            assert_eq!(parse_rfc3339(input), Some(want), "{input}");
        }
    }

    #[test]
    fn rejects_malformed_timestamps() {
        for input in [
            "",
            "2026-10-17",
            "2026-10-17T07:59:59",
            "2026-10-17 07:59:59Z",
            "2026-13-01T00:00:00Z",
            "2026-10-17T24:00:00Z",
            "2026-10-17T07:59:59.Z",
            "2026-10-17T07:59:59+0000",
            "2026-10-17T07:59:59+00:00x",
            "üü-10-17T07:59:59Z",
        ] {
            assert_eq!(parse_rfc3339(input), None, "{input}");
        }
    }

    #[test]
    fn formats_utc_and_round_trips() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_792_200_000), "2026-10-17T01:20:00Z");
        for unix in [-1, 0, 951_825_600, 1_792_228_473, 4_102_444_800] {
            assert_eq!(parse_rfc3339(&format_rfc3339(unix)), Some(unix), "{unix}");
        }
    }
}
