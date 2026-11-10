//! Formatting of times, durations and sizes for the pages.

use time::OffsetDateTime;
use time::macros::format_description;

use crate::i18n::{Lang, Tr};

/// `2026-11-10 19:04 UTC`.
pub fn absolute(ts: i64) -> String {
    OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| {
            t.format(format_description!(
                "[year]-[month]-[day] [hour]:[minute] UTC"
            ))
            .ok()
        })
        .unwrap_or_default()
}

/// Steps of the relative time text: the limit in seconds, the unit in
/// seconds, and the past and future message keys.
const RELATIVE_STEPS: [(i64, i64, &str, &str); 3] = [
    (3_600, 60, "time.minutes_ago", "time.in_minutes"),
    (86_400, 3_600, "time.hours_ago", "time.in_hours"),
    (i64::MAX, 86_400, "time.days_ago", "time.in_days"),
];

/// `5 dk önce` / `in 2 h`, relative to `now`.
pub fn relative(tr: &Tr, now: i64, ts: i64) -> String {
    let diff = now - ts;
    let distance = diff.saturating_abs();
    if distance < 60 {
        let key = if diff >= 0 {
            "time.just_now"
        } else {
            "time.soon"
        };
        return tr.t(key).to_owned();
    }
    let (_, unit, past, future) = RELATIVE_STEPS
        .into_iter()
        .find(|(limit, ..)| distance < *limit)
        .unwrap_or(RELATIVE_STEPS[2]);
    let key = if diff >= 0 { past } else { future };
    tr.tf(key, &[("n", &(distance / unit).to_string())])
}

/// `12 sn`, `3 dk 5 sn`.
pub fn duration(tr: &Tr, seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return tr.tf("time.seconds", &[("n", &seconds.to_string())]);
    }
    tr.tf(
        "time.minutes_seconds",
        &[
            ("m", &(seconds / 60).to_string()),
            ("s", &(seconds % 60).to_string()),
        ],
    )
}

/// `1,2 MB` in Turkish, `1.2 MB` in English.
pub fn bytes(lang: Lang, size: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let text = if unit == 0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    };
    let text = match lang {
        Lang::Tr => text.replace('.', ","),
        Lang::En => text,
    };
    format!("{text} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Catalog;
    use std::sync::Arc;

    fn tr(lang: Lang) -> Tr {
        Tr::new(Arc::new(Catalog::load().unwrap()), lang)
    }

    #[test]
    fn absolute_time_is_utc() {
        assert_eq!(absolute(0), "1970-01-01 00:00 UTC");
        // 1_700_000_000 is 2023-11-14T22:13:20Z.
        assert_eq!(absolute(1_700_000_000), "2023-11-14 22:13 UTC");
    }

    #[test]
    fn relative_time_uses_the_largest_whole_unit() {
        let tr = tr(Lang::En);
        let now = 1_000_000;
        assert_eq!(relative(&tr, now, now - 30), "just now");
        assert_eq!(relative(&tr, now, now - 300), "5 min ago");
        assert_eq!(relative(&tr, now, now - 7_200), "2 h ago");
        assert_eq!(relative(&tr, now, now - 3 * 86_400), "3 days ago");
        assert_eq!(relative(&tr, now, now + 3_600 * 5), "in 5 h");
        assert_eq!(relative(&tr, now, now + 10), "soon");
    }

    #[test]
    fn relative_time_is_translated() {
        assert_eq!(relative(&tr(Lang::Tr), 1_000, 1_000 - 120), "2 dk önce");
    }

    #[test]
    fn durations_and_sizes() {
        let tr = tr(Lang::En);
        assert_eq!(duration(&tr, 12), "12 s");
        assert_eq!(duration(&tr, 185), "3 min 5 s");
        assert_eq!(bytes(Lang::En, 512), "512 B");
        assert_eq!(bytes(Lang::En, 1_572_864), "1.5 MB");
        assert_eq!(bytes(Lang::Tr, 1_572_864), "1,5 MB");
    }
}
