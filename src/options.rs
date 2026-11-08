use clap::ValueEnum;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

pub const COMMIT_MESSAGE: &str = "Aether Auto-Sync";
pub const DEFAULT_EXCLUDES: [&str; 3] = [".git", "bin", "obj"];
pub const DEFAULT_DELAY: &str = "10m";

/// Silence required before an immediate sync, so one save is not split into many commits.
pub const QUIET_WINDOW: Duration = Duration::from_secs(2);
/// Minimum gap between two `never` reminders.
pub const REMINDER_THROTTLE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Strategy {
    Immediate,
    Delay,
    Never,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Immediate => "immediate",
            Strategy::Delay => "delay",
            Strategy::Never => "never",
        }
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub path: PathBuf,
    pub strategy: Strategy,
    pub delay: Duration,
    pub excludes: Vec<String>,
}

impl Options {
    /// Time the engine waits after the most recent change before syncing.
    pub fn trigger_window(&self) -> Duration {
        match self.strategy {
            Strategy::Immediate => QUIET_WINDOW,
            _ => self.delay,
        }
    }

    pub fn strategy_label(&self) -> String {
        match self.strategy {
            Strategy::Immediate => "immediate".to_string(),
            Strategy::Delay => format!("delay ({})", format_duration(self.delay)),
            Strategy::Never => "never".to_string(),
        }
    }
}

/// Parses `10m`, `30s`, `2h`, `500ms`, `1.5m`. A bare number means minutes.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty duration".to_string());
    }
    let split = trimmed
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let amount: f64 = number
        .parse()
        .map_err(|_| format!("invalid duration '{text}'"))?;
    if !amount.is_finite() || amount < 0.0 {
        return Err(format!("invalid duration '{text}'"));
    }

    let factor = match unit.trim().to_ascii_lowercase().as_str() {
        "ms" => 0.001,
        "s" => 1.0,
        "" | "m" => 60.0,
        "h" => 3_600.0,
        "d" => 86_400.0,
        other => return Err(format!("unknown duration unit '{other}' in '{text}'")),
    };
    let seconds = amount * factor;
    if seconds > f64::from(u32::MAX) {
        return Err(format!("duration '{text}' is too long"));
    }
    Ok(Duration::from_secs_f64(seconds))
}

pub fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis == 0 {
        return "0s".to_string();
    }
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    let secs = duration.as_secs();
    if secs >= 86_400 && secs % 86_400 == 0 {
        return format!("{}d", secs / 86_400);
    }
    if secs >= 3_600 && secs % 3_600 == 0 {
        return format!("{}h", secs / 3_600);
    }
    if secs >= 60 && secs % 60 == 0 {
        return format!("{}m", secs / 60);
    }
    format!("{secs}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1.5m").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("15").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn rejects_invalid_durations() {
        for bad in ["", "abc", "-1m", "10x", "1..5m", "NaN", "999999999999d"] {
            assert!(parse_duration(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::from_secs(600)), "10m");
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h");
    }

    #[test]
    fn immediate_uses_quiet_window() {
        let opts = Options {
            path: PathBuf::from("."),
            strategy: Strategy::Immediate,
            delay: Duration::from_secs(600),
            excludes: vec![],
        };
        assert_eq!(opts.trigger_window(), QUIET_WINDOW);
    }
}
