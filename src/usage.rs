//! Subscription usage limits and how `credhop usage` and `credhop auto` print them.

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

impl Usage {
    /// The percentage left in the window with the least left, as it is shown;
    /// 100 without windows.
    pub fn least_left(&self) -> i64 {
        self.windows.iter().map(left_percent).min().unwrap_or(100)
    }
}

/// How many characters wide the bar of a table cell is.
const BAR_WIDTH: usize = 10;

/// How much of the limit is left, as a whole percentage from 0 to 100.
fn left_percent(window: &Window) -> i64 {
    // The cast also turns a rounded -0.0 into 0.
    (100.0 - window.used_percent).round().clamp(0.0, 100.0) as i64
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
    let mut text = format!("{} {}% left", window.label, left_percent(window));
    if let Some(resets_in) = resets_in(window, now) {
        text.push_str(&format!(" (resets in {resets_in})"));
    }
    text
}

/// `████████··  77% (2h13m)`: a bar of how much of `window` is left, the
/// percentage and the time to its reset, for a table column. With `color`, the
/// bar and the percentage are red, yellow or green by how much is left.
pub fn format_cell(window: &Window, now: i64, color: bool) -> String {
    let left = left_percent(window);
    // Rounded up, so that anything left shows.
    let filled = (left as usize * BAR_WIDTH).div_ceil(100);
    let empty = BAR_WIDTH - filled;
    let mut text = format!("{}{} {left:>3}%", "█".repeat(filled), "·".repeat(empty));
    if color {
        text = format!("\x1b[{}m{text}\x1b[0m", color_code(left));
    }
    if let Some(resets_in) = resets_in(window, now) {
        text.push_str(&format!(" ({resets_in})"));
    }
    text
}

/// The ANSI color of a limit with `left` percent left: red at 10 or less,
/// where `auto` switches by default, yellow up to 30 and green above.
fn color_code(left: i64) -> u8 {
    match left {
        ..=10 => 31,
        11..=30 => 33,
        _ => 32,
    }
}

/// The time until `window` resets, such as `2h13m`, unless it is unknown or past.
fn resets_in(window: &Window, now: i64) -> Option<String> {
    window
        .resets_at
        .filter(|&resets_at| resets_at > now)
        .map(|resets_at| format_duration(resets_at - now))
}

/// `3d4h`, `2h13m`, `45m` or `<1m`.
pub fn format_duration(seconds: i64) -> String {
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
    fn least_left_is_the_percentage_shown_for_the_fullest_window() {
        let usage = |used: &[f64]| Usage {
            windows: used.iter().map(|&used| window("5h", used, None)).collect(),
        };

        assert_eq!(usage(&[23.4, 91.0, 50.0]).least_left(), 9);
        assert_eq!(usage(&[89.6]).least_left(), 10); // as `10% left` is shown
        assert_eq!(usage(&[130.0, 20.0]).least_left(), 0);
        assert_eq!(usage(&[]).least_left(), 100);
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

    #[test]
    fn cells_draw_a_bar_of_what_is_left_before_the_percentage_and_the_time_to_reset() {
        let now = 1_000_000;
        let cell = |used, resets_at| format_cell(&window("5h", used, resets_at), now, false);

        assert_eq!(cell(23.4, Some(now + 7980)), "████████··  77% (2h13m)");
        assert_eq!(cell(0.0, Some(now + 60)), "██████████ 100% (1m)");
        assert_eq!(cell(99.0, None), "█·········   1%"); // anything left shows
        assert_eq!(cell(100.0, None), "··········   0%");
        assert_eq!(cell(50.0, Some(now)), "█████·····  50%"); // a past reset time is omitted
    }

    #[test]
    fn cells_are_colored_by_how_much_is_left() {
        let now = 1_000_000;
        let cell =
            |left: f64, resets_at| format_cell(&window("5h", 100.0 - left, resets_at), now, true);

        assert_eq!(cell(10.0, None), "\x1b[31m█·········  10%\x1b[0m");
        assert_eq!(cell(11.0, None), "\x1b[33m██········  11%\x1b[0m");
        assert_eq!(cell(30.0, None), "\x1b[33m███·······  30%\x1b[0m");
        assert_eq!(
            cell(31.0, Some(now + 60)),
            "\x1b[32m████······  31%\x1b[0m (1m)"
        );
    }
}
