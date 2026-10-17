//! Subscription usage limits and how `credhop usage` prints them.

/// One usage limit window, such as the 5-hour or the weekly limit.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// Shown before the percentage, e.g. `5h` or `7d Fable`.
    pub label: String,
    /// How much of the limit is used, from 0 to 100.
    pub used_percent: f64,
    /// When the window resets, in Unix seconds.
    pub resets_at: Option<i64>,
}

/// The usage limits of one account.
#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub windows: Vec<Window>,
}

/// `5h 77% left (resets in 2h13m)  7d 59% left`: the windows, two spaces apart.
pub fn format_windows(windows: &[Window], now: i64) -> String {
    windows
        .iter()
        .map(|window| format_window(window, now))
        .collect::<Vec<_>>()
        .join("  ")
}

fn format_window(window: &Window, now: i64) -> String {
    // The cast also turns a rounded -0.0 into 0.
    let left = (100.0 - window.used_percent).round().clamp(0.0, 100.0) as i64;
    let mut text = format!("{} {left}% left", window.label);
    if let Some(resets_at) = window.resets_at
        && resets_at > now
    {
        text.push_str(&format!(
            " (resets in {})",
            format_duration(resets_at - now)
        ));
    }
    text
}

/// `3d4h`, `2h13m`, `45m` or `<1m`.
fn format_duration(seconds: i64) -> String {
    let minutes = seconds / 60;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "<1m".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(label: &str, used_percent: f64, resets_at: Option<i64>) -> Window {
        Window {
            label: label.to_owned(),
            used_percent,
            resets_at,
        }
    }

    #[test]
    fn durations_use_the_two_largest_units() {
        let cases = [
            (3 * 86_400 + 4 * 3600 + 59 * 60, "3d4h"),
            (86_400, "1d0h"),
            (2 * 3600 + 13 * 60 + 5, "2h13m"),
            (3600, "1h0m"),
            (45 * 60 + 59, "45m"),
            (59, "<1m"),
        ];
        for (seconds, want) in cases {
            assert_eq!(format_duration(seconds), want, "{seconds}");
        }
    }

    #[test]
    fn windows_show_the_percentage_left_and_the_time_to_reset() {
        let now = 1_000_000;
        let windows = [
            window("5h", 23.4, Some(now + 7980)),
            window("7d Fable", 0.0, None),
        ];

        assert_eq!(
            format_windows(&windows, now),
            "5h 77% left (resets in 2h13m)  7d Fable 100% left"
        );
    }

    #[test]
    fn percentage_left_is_rounded_and_clamped() {
        let left = |used| format_windows(&[window("5h", used, None)], 0);

        assert_eq!(left(22.5), "5h 78% left"); // 77.5 rounds away from zero
        assert_eq!(left(100.4), "5h 0% left");
        assert_eq!(left(130.0), "5h 0% left");
        assert_eq!(left(-5.0), "5h 100% left");
    }

    #[test]
    fn past_reset_times_are_omitted() {
        let now = 1_000_000;

        assert_eq!(
            format_windows(&[window("5h", 50.0, Some(now))], now),
            "5h 50% left"
        );
        assert_eq!(
            format_windows(&[window("5h", 50.0, Some(now - 1))], now),
            "5h 50% left"
        );
        assert_eq!(format_windows(&[], now), "");
    }
}
