//! Markup for the timestamps the dashboard shows. SQLite stores every time in
//! UTC, so the server wraps each one in a `<time>` element: the `datetime`
//! attribute carries the UTC time in ISO 8601 with a `Z` suffix, and the
//! element's own text is the UTC time the page showed before, so a visitor
//! with JavaScript off still reads a correct time. `static/local-time.js`
//! rewrites the text to the browser's own time zone, because only the browser
//! knows where its visitor is.

use maud::{Markup, html};

/// Which part of a time an element shows, matched by `local-time.js`.
enum Form {
    /// Date and time, labelled with the zone, such as `2026-11-20 09:15:00`.
    DateTime,
    /// `HH:MM`.
    Time,
    /// `HH:MM:SS`.
    TimeSeconds,
}

impl Form {
    fn attr(&self) -> &'static str {
        match self {
            Form::DateTime => "datetime",
            Form::Time => "time",
            Form::TimeSeconds => "time-seconds",
        }
    }
}

/// The ISO 8601 form with a `Z` suffix of a SQLite `YYYY-MM-DD HH:MM:SS`
/// string, or `None` when the string is not that shape.
fn iso(at: &str) -> Option<String> {
    let date = at.get(0..10)?;
    let time = at.get(11..19)?;
    (at.as_bytes().get(10) == Some(&b' ')).then(|| format!("{date}T{time}Z"))
}

fn element(iso: &str, form: &Form, text: &str) -> Markup {
    html! { time datetime=(iso) data-format=(form.attr()) { (text) } }
}

/// Wraps the `text` of a stored time in a `<time>` element, or renders the
/// plain `text` when the time is not the shape `iso` expects.
fn wrap(at: &str, form: &Form, text: &str) -> Markup {
    if let Some(iso) = iso(at) {
        element(&iso, form, text)
    } else {
        html! { (text) }
    }
}

/// A stored `YYYY-MM-DD HH:MM:SS` time as a full date and time. Without
/// JavaScript it reads `YYYY-MM-DD HH:MM:SS UTC`.
pub(crate) fn date_time(at: &str) -> Markup {
    wrap(at, &Form::DateTime, &format!("{at} UTC"))
}

/// A stored time as `HH:MM`, with no zone label, as the timeline shows it.
pub(crate) fn time(at: &str) -> Markup {
    wrap(at, &Form::Time, at.get(11..16).unwrap_or(at))
}

/// A stored time as `HH:MM:SS`, with no zone label, as the log shows it.
pub(crate) fn time_seconds(at: &str) -> Markup {
    wrap(at, &Form::TimeSeconds, at.get(11..).unwrap_or(at))
}

/// A Unix time in seconds as a full date and time at minute precision, as the
/// plan usage tile shows its report and reset times. Without JavaScript it
/// reads `YYYY-MM-DD HH:MM UTC`.
pub(crate) fn unix(unix: i64) -> Markup {
    let (year, month, day) = civil_from_days(unix.div_euclid(86_400));
    let secs = unix.rem_euclid(86_400);
    let (hour, minute) = (secs / 3600, secs % 3600 / 60);
    let iso = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00Z");
    let text = format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC");
    element(&iso, &Form::DateTime, &text)
}

/// The civil date (year, month 1 to 12, day 1 to 31) for `days` days after
/// 1970-01-01, by Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}
