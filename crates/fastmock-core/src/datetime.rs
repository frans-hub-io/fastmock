use chrono::format::{Item, StrftimeItems};
use chrono::{
    DateTime, Datelike, FixedOffset, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta,
    TimeZone, Utc,
};

use crate::model::DateTruncation;

pub const MAX_FORMAT_LEN: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Millis,
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
    Years,
}

pub fn parse_unit(s: &str) -> Option<Unit> {
    Some(match s.to_ascii_lowercase().as_str() {
        "ms" | "milli" | "millis" | "millisecond" | "milliseconds" => Unit::Millis,
        "s" | "sec" | "secs" | "second" | "seconds" => Unit::Seconds,
        "min" | "mins" | "minute" | "minutes" => Unit::Minutes,
        "h" | "hour" | "hours" => Unit::Hours,
        "d" | "day" | "days" => Unit::Days,
        "w" | "week" | "weeks" => Unit::Weeks,
        "month" | "months" => Unit::Months,
        "y" | "year" | "years" => Unit::Years,
        _ => return None,
    })
}

pub fn apply_offset(t: DateTime<Utc>, amount: i64, unit: Unit) -> Option<DateTime<Utc>> {
    let delta = |d: Option<TimeDelta>| d.and_then(|d| t.checked_add_signed(d));
    match unit {
        Unit::Millis => delta(TimeDelta::try_milliseconds(amount)),
        Unit::Seconds => delta(TimeDelta::try_seconds(amount)),
        Unit::Minutes => delta(TimeDelta::try_minutes(amount)),
        Unit::Hours => delta(TimeDelta::try_hours(amount)),
        Unit::Days => delta(TimeDelta::try_days(amount)),
        Unit::Weeks => delta(TimeDelta::try_weeks(amount)),
        Unit::Months | Unit::Years => {
            let months = if unit == Unit::Years {
                amount.checked_mul(12)?
            } else {
                amount
            };
            let m = Months::new(u32::try_from(months.unsigned_abs()).ok()?);
            if months >= 0 {
                t.checked_add_months(m)
            } else {
                t.checked_sub_months(m)
            }
        }
    }
}

pub fn parse_offset(s: &str) -> Result<(i64, Unit), String> {
    let s = s.trim();
    let digits_end = s
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && (*c == '+' || *c == '-')) || *c == ' '))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(digits_end);
    let num: String = num.chars().filter(|c| !c.is_whitespace()).collect();
    let amount = num
        .parse::<i64>()
        .map_err(|_| format!("invalid offset `{s}`: expected e.g. `3 days` or `-1 hours`"))?;
    let unit = parse_unit(unit.trim()).ok_or_else(|| format!("invalid offset unit in `{s}`"))?;
    Ok((amount, unit))
}

fn midnight(d: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&d.and_time(NaiveTime::MIN))
}

pub fn truncate(t: DateTime<Utc>, tr: DateTruncation) -> DateTime<Utc> {
    let d = t.date_naive();
    let first_of_month = NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d);
    let next_month = first_of_month
        .checked_add_months(Months::new(1))
        .unwrap_or(first_of_month);
    match tr {
        DateTruncation::FirstSecondOfMinute => t
            .with_timezone(&Utc)
            .date_naive()
            .and_hms_opt(
                chrono::Timelike::hour(&t),
                chrono::Timelike::minute(&t),
                chrono::Timelike::second(&t),
            )
            .map(|n| Utc.from_utc_datetime(&n))
            .unwrap_or(t),
        DateTruncation::FirstMinuteOfHour => d
            .and_hms_opt(chrono::Timelike::hour(&t), 0, 0)
            .map(|n| Utc.from_utc_datetime(&n))
            .unwrap_or(t),
        DateTruncation::FirstHourOfDay => midnight(d),
        DateTruncation::FirstDayOfMonth => midnight(first_of_month),
        DateTruncation::FirstDayOfNextMonth => midnight(next_month),
        DateTruncation::LastDayOfMonth => midnight(next_month.pred_opt().unwrap_or(d)),
        DateTruncation::FirstDayOfYear => {
            midnight(NaiveDate::from_ymd_opt(d.year(), 1, 1).unwrap_or(d))
        }
        DateTruncation::FirstDayOfNextYear => {
            midnight(NaiveDate::from_ymd_opt(d.year() + 1, 1, 1).unwrap_or(d))
        }
        DateTruncation::LastDayOfYear => {
            midnight(NaiveDate::from_ymd_opt(d.year(), 12, 31).unwrap_or(d))
        }
    }
}

pub fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 {
        return None;
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    if let Ok(t) = DateTime::parse_from_rfc2822(s) {
        return Some(t.with_timezone(&Utc));
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f%z"] {
        if let Ok(t) = DateTime::parse_from_str(s, f) {
            return Some(t.with_timezone(&Utc));
        }
    }
    for f in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, f) {
            return Some(Utc.from_utc_datetime(&t));
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(midnight)
}

pub fn java_to_strftime(fmt: &str) -> Result<String, String> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            if chars.get(i + 1) == Some(&'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                if chars[i] == '%' {
                    out.push_str("%%");
                } else {
                    out.push(chars[i]);
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if !c.is_ascii_alphabetic() {
            if c == '%' {
                out.push_str("%%");
            } else {
                out.push(c);
            }
            i += 1;
            continue;
        }
        let mut n = 1;
        while chars.get(i + n) == Some(&c) {
            n += 1;
        }
        i += n;
        let spec = match (c, n) {
            ('y' | 'u', 2) => "%y",
            ('y' | 'u', _) => "%Y",
            ('M', 1) => "%-m",
            ('M', 2) => "%m",
            ('M', 3) => "%b",
            ('M', _) => "%B",
            ('d', 1) => "%-d",
            ('d', _) => "%d",
            ('D', _) => "%j",
            ('H', 1) => "%-H",
            ('H', _) => "%H",
            ('h', 1) => "%-I",
            ('h', _) => "%I",
            ('k', _) => "%k",
            ('m', 1) => "%-M",
            ('m', _) => "%M",
            ('s', 1) => "%-S",
            ('s', _) => "%S",
            ('S', 6) => "%6f",
            ('S', 9) => "%9f",
            ('S', _) => "%3f",
            ('E', 1..=3) => "%a",
            ('E', _) => "%A",
            ('a', _) => "%p",
            ('Z', 1..=3) => "%z",
            ('Z', _) => "%:z",
            ('X' | 'x', 1 | 2) => "%z",
            ('X' | 'x', _) => "%:z",
            ('z', _) => "%Z",
            ('w', _) => "%V",
            ('e' | 'c', _) => "%u",
            _ => return Err(format!("unsupported date pattern letter `{c}`")),
        };
        out.push_str(spec);
    }
    Ok(out)
}

fn strftime_pattern(fmt: &str) -> Result<String, String> {
    if fmt.len() > MAX_FORMAT_LEN {
        return Err(format!("date format longer than {MAX_FORMAT_LEN} bytes"));
    }
    let f = if fmt.contains('%') {
        fmt.to_string()
    } else {
        java_to_strftime(fmt)?
    };
    if StrftimeItems::new(&f).any(|i| matches!(i, Item::Error)) {
        return Err(format!("invalid date format `{fmt}`"));
    }
    Ok(f)
}

pub fn validate_format(fmt: &str) -> Result<(), String> {
    match fmt {
        "iso" | "isoSeconds" | "isoNanos" | "epoch" | "unix" | "epochMillis" => Ok(()),
        f => strftime_pattern(f).map(|_| ()),
    }
}

pub fn parse_with_format(s: &str, fmt: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    match fmt {
        "unix" | "epoch" => {
            return s
                .parse::<i64>()
                .ok()
                .and_then(|v| Utc.timestamp_opt(v, 0).single());
        }
        "epochMillis" => {
            return s
                .parse::<i64>()
                .ok()
                .and_then(|v| Utc.timestamp_millis_opt(v).single());
        }
        "iso" | "isoSeconds" | "isoNanos" => return parse_datetime(s),
        _ => {}
    }
    let f = strftime_pattern(fmt).ok()?;
    if let Ok(t) = DateTime::parse_from_str(s, &f) {
        return Some(t.with_timezone(&Utc));
    }
    if let Ok(t) = NaiveDateTime::parse_from_str(s, &f) {
        return Some(Utc.from_utc_datetime(&t));
    }
    NaiveDate::parse_from_str(s, &f).ok().map(midnight)
}

pub fn parse_timezone(tz: &str) -> Option<FixedOffset> {
    let t = tz.trim();
    if matches!(
        t.to_ascii_uppercase().as_str(),
        "UTC" | "GMT" | "Z" | "ETC/UTC"
    ) {
        return FixedOffset::east_opt(0);
    }
    let t = t
        .strip_prefix("UTC")
        .or_else(|| t.strip_prefix("GMT"))
        .unwrap_or(t);
    let (sign, rest) = match t.chars().next()? {
        '+' => (1, &t[1..]),
        '-' => (-1, &t[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.is_empty() || digits.len() > 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (h, m) = if digits.len() <= 2 {
        (digits.parse::<i32>().ok()?, 0)
    } else {
        let (h, m) = digits.split_at(digits.len() - 2);
        (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?)
    };
    if h > 18 || m > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60))
}

pub fn format(
    t: DateTime<Utc>,
    fmt: Option<&str>,
    tz: Option<FixedOffset>,
) -> Result<String, String> {
    use std::fmt::Write as _;
    let local =
        t.with_timezone(&tz.unwrap_or_else(|| FixedOffset::east_opt(0).expect("zero offset")));
    match fmt {
        None | Some("iso") => Ok(if tz.is_some_and(|z| z.local_minus_utc() != 0) {
            local.to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
        } else {
            t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        }),
        Some("isoSeconds") => Ok(if tz.is_some_and(|z| z.local_minus_utc() != 0) {
            local.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
        } else {
            t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }),
        Some("isoNanos") => Ok(t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
        Some("epochMillis") => Ok(t.timestamp_millis().to_string()),
        Some("epoch") | Some("unix") => Ok(t.timestamp().to_string()),
        Some(f) => {
            let pattern = strftime_pattern(f)?;
            let mut out = String::new();
            write!(out, "{}", local.format(&pattern))
                .map_err(|_| format!("invalid date format `{f}`"))?;
            Ok(out)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExpectedTime {
    fixed: Option<DateTime<Utc>>,
    offset: Option<(i64, Unit)>,
}

impl ExpectedTime {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.len() > 128 {
            return Err("date-time expression is too long".into());
        }
        static TAIL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"^(.*?)\s*([+-]\s*\d+\s*[A-Za-z]+)$").expect("static regex")
        });
        let (base, rest) = if s.len() >= 3 && s[..3].eq_ignore_ascii_case("now") {
            (&s[..3], s[3..].trim())
        } else if parse_datetime(s).is_some() {
            (s, "")
        } else {
            match TAIL.captures(s) {
                Some(c) => (
                    c.get(1).map(|m| m.as_str()).unwrap_or(""),
                    c.get(2).map(|m| m.as_str()).unwrap_or(""),
                ),
                None => (s, ""),
            }
        };
        let fixed = if base.eq_ignore_ascii_case("now") {
            None
        } else {
            Some(
                parse_datetime(base)
                    .ok_or_else(|| format!("`{s}` is not `now` or an ISO-8601 date-time"))?,
            )
        };
        let offset = if rest.is_empty() {
            None
        } else {
            Some(parse_offset(rest)?)
        };
        Ok(Self { fixed, offset })
    }

    pub fn with_offset(mut self, amount: i64, unit: Unit) -> Self {
        self.offset = Some((amount, unit));
        self
    }

    pub fn resolve(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let base = self.fixed.unwrap_or(now);
        match self.offset {
            Some((a, u)) => apply_offset(base, a, u),
            None => Some(base),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        parse_datetime(s).unwrap()
    }

    #[test]
    fn parses_common_formats() {
        assert_eq!(at("2021-06-14T10:00:00Z"), at("2021-06-14T12:00:00+02:00"));
        assert_eq!(
            at("Mon, 14 Jun 2021 10:00:00 GMT"),
            at("2021-06-14T10:00:00Z")
        );
        assert_eq!(at("2021-06-14"), at("2021-06-14T00:00:00Z"));
        assert_eq!(at("2021-06-14 10:00:00"), at("2021-06-14T10:00:00Z"));
        assert!(parse_datetime("yesterday").is_none());
    }

    #[test]
    fn expected_expressions() {
        let now = at("2021-01-31T10:00:00Z");
        assert_eq!(ExpectedTime::parse("now").unwrap().resolve(now), Some(now));
        assert_eq!(
            ExpectedTime::parse("now +3 days").unwrap().resolve(now),
            Some(at("2021-02-03T10:00:00Z"))
        );
        assert_eq!(
            ExpectedTime::parse("now -1 hours").unwrap().resolve(now),
            Some(at("2021-01-31T09:00:00Z"))
        );
        assert_eq!(
            ExpectedTime::parse("now +1 months").unwrap().resolve(now),
            Some(at("2021-02-28T10:00:00Z"))
        );
        assert_eq!(
            ExpectedTime::parse("2021-06-01T00:00:00Z")
                .unwrap()
                .resolve(now),
            Some(at("2021-06-01T00:00:00Z"))
        );
        assert_eq!(
            ExpectedTime::parse("2021-06-01T00:00:00Z +2 hours")
                .unwrap()
                .resolve(now),
            Some(at("2021-06-01T02:00:00Z"))
        );
        assert!(ExpectedTime::parse("now +3 fortnights").is_err());
        assert!(ExpectedTime::parse("tomorrow").is_err());
    }

    #[test]
    fn truncation() {
        let t = at("2021-02-14T10:42:17.5Z");
        assert_eq!(
            truncate(t, DateTruncation::FirstSecondOfMinute),
            at("2021-02-14T10:42:17Z")
        );
        assert_eq!(
            truncate(t, DateTruncation::FirstMinuteOfHour),
            at("2021-02-14T10:00:00Z")
        );
        assert_eq!(
            truncate(t, DateTruncation::FirstHourOfDay),
            at("2021-02-14T00:00:00Z")
        );
        assert_eq!(
            truncate(t, DateTruncation::FirstDayOfMonth),
            at("2021-02-01T00:00:00Z")
        );
        assert_eq!(
            truncate(t, DateTruncation::FirstDayOfNextMonth),
            at("2021-03-01T00:00:00Z")
        );
        assert_eq!(
            truncate(t, DateTruncation::LastDayOfMonth),
            at("2021-02-28T00:00:00Z")
        );
        assert_eq!(
            truncate(t, DateTruncation::FirstDayOfYear),
            at("2021-01-01T00:00:00Z")
        );
        assert_eq!(
            truncate(t, DateTruncation::FirstDayOfNextYear),
            at("2022-01-01T00:00:00Z")
        );
        assert_eq!(
            truncate(t, DateTruncation::LastDayOfYear),
            at("2021-12-31T00:00:00Z")
        );
    }

    #[test]
    fn java_patterns_and_formatting() {
        assert_eq!(
            java_to_strftime("yyyy-MM-dd'T'HH:mm:ss").unwrap(),
            "%Y-%m-%dT%H:%M:%S"
        );
        assert!(java_to_strftime("yyyy-qq").is_err());
        let t = at("2021-02-04T05:06:07.089Z");
        assert_eq!(format(t, Some("dd/MM/yyyy"), None).unwrap(), "04/02/2021");
        assert_eq!(format(t, Some("%Y"), None).unwrap(), "2021");
        assert_eq!(format(t, Some("unix"), None).unwrap(), "1612415167");
        assert_eq!(format(t, Some("epoch"), None).unwrap(), "1612415167");
        assert_eq!(
            format(t, Some("epochMillis"), None).unwrap(),
            "1612415167089"
        );
        assert_eq!(
            format(t, Some("HH:mm"), parse_timezone("+03:00")).unwrap(),
            "08:06"
        );
        assert_eq!(
            format(t, None, parse_timezone("-0130")).unwrap(),
            "2021-02-04T03:36:07.089-01:30"
        );
        assert!(parse_timezone("Europe/Kyiv").is_none());
        assert!(format(t, Some("%Q"), None).is_err());
        assert_eq!(
            parse_with_format("04/02/2021", "dd/MM/yyyy"),
            Some(at("2021-02-04"))
        );
        assert_eq!(
            parse_with_format("1612415167", "unix"),
            Some(at("2021-02-04T05:06:07Z"))
        );
        assert_eq!(parse_offset("-24 seconds").unwrap(), (-24, Unit::Seconds));
        assert_eq!(parse_offset("3days").unwrap(), (3, Unit::Days));
    }
}
