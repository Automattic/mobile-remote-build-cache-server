//! Minimal JSON-lines logger: one object per line on stdout.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static ENABLED: AtomicBool = AtomicBool::new(true);

/// Turns all output on or off (tests run with it off).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// A log field value.
pub enum V<'a> {
    S(&'a str),
    U(u64),
    I(i64),
    F(f64),
    B(bool),
}

impl<'a> From<&'a str> for V<'a> {
    fn from(v: &'a str) -> Self {
        V::S(v)
    }
}
impl<'a> From<&'a String> for V<'a> {
    fn from(v: &'a String) -> Self {
        V::S(v)
    }
}
impl From<u64> for V<'_> {
    fn from(v: u64) -> Self {
        V::U(v)
    }
}
impl From<usize> for V<'_> {
    fn from(v: usize) -> Self {
        V::U(v as u64)
    }
}
impl From<u32> for V<'_> {
    fn from(v: u32) -> Self {
        V::U(v as u64)
    }
}
impl From<u16> for V<'_> {
    fn from(v: u16) -> Self {
        V::U(v as u64)
    }
}
impl From<i64> for V<'_> {
    fn from(v: i64) -> Self {
        V::I(v)
    }
}
impl From<f64> for V<'_> {
    fn from(v: f64) -> Self {
        V::F(v)
    }
}
impl From<bool> for V<'_> {
    fn from(v: bool) -> Self {
        V::B(v)
    }
}

pub fn info(msg: &str, fields: &[(&str, V)]) {
    emit("info", msg, fields);
}

pub fn warn(msg: &str, fields: &[(&str, V)]) {
    emit("warn", msg, fields);
}

pub fn error(msg: &str, fields: &[(&str, V)]) {
    emit("error", msg, fields);
}

fn emit(level: &str, msg: &str, fields: &[(&str, V)]) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let mut line = format_line(SystemTime::now(), level, msg, fields);
    line.push('\n');
    // A closed stdout must not take the server down.
    let _ = std::io::stdout().lock().write_all(line.as_bytes());
}

fn format_line(now: SystemTime, level: &str, msg: &str, fields: &[(&str, V)]) -> String {
    let mut out = String::with_capacity(128);
    out.push_str("{\"ts\":\"");
    write_timestamp(&mut out, now);
    out.push_str("\",\"level\":\"");
    out.push_str(level);
    out.push_str("\",\"msg\":");
    write_json_string(&mut out, msg);
    for (key, value) in fields {
        out.push(',');
        write_json_string(&mut out, key);
        out.push(':');
        match value {
            V::S(s) => write_json_string(&mut out, s),
            V::U(n) => {
                let _ = write!(out, "{n}");
            }
            V::I(n) => {
                let _ = write!(out, "{n}");
            }
            V::F(f) if f.is_finite() => {
                let _ = write!(out, "{f}");
            }
            V::F(_) => out.push_str("null"),
            V::B(b) => out.push_str(if *b { "true" } else { "false" }),
        }
    }
    out.push('}');
    out
}

fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// RFC 3339 UTC timestamp with millisecond precision.
fn write_timestamp(out: &mut String, t: SystemTime) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, day) = civil_from_days(days as i64);
    let _ = write!(
        out,
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    );
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_a_line() {
        let t = UNIX_EPOCH + Duration::from_millis(1_795_441_645_123);
        let line = format_line(
            t,
            "info",
            "hello \"world\"\n",
            &[
                ("key", "a\\b\u{1}".into()),
                ("n", 42u64.into()),
                ("neg", (-3i64).into()),
                ("ok", true.into()),
                ("f", 0.5f64.into()),
                ("nan", f64::NAN.into()),
            ],
        );
        assert_eq!(
            line,
            "{\"ts\":\"2026-11-23T13:47:25.123Z\",\"level\":\"info\",\"msg\":\"hello \\\"world\\\"\\n\",\
             \"key\":\"a\\\\b\\u0001\",\"n\":42,\"neg\":-3,\"ok\":true,\"f\":0.5,\"nan\":null}"
        );
    }

    #[test]
    fn calendar() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_088), (2024, 12, 31));
    }
}
