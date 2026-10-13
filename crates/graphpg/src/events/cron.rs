//! Cron schedules: the standard 5 fields (`minute hour day-of-month month day-of-week`), in
//! UTC, each `*`, a value, a range `a-b`, a step `*/n` or `a-b/n`, or a list of those; and the
//! aliases `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`. As in cron, when both the day
//! of the month and the day of the week are restricted, a day matching either is due.
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};

/// A parsed schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    minutes: [bool; 60],
    hours: [bool; 24],
    days: [bool; 32],
    months: [bool; 13],
    weekdays: [bool; 7],
    /// Whether the day of the month / of the week is restricted (not `*`).
    days_restricted: bool,
    weekdays_restricted: bool,
}

impl Schedule {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let expanded = match text {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err(format!(
                "`{text}`: a schedule has 5 fields (minute hour day-of-month month day-of-week) or is @hourly, @daily, @weekly, @monthly"
            ));
        };
        let mut s = Self {
            minutes: [false; 60],
            hours: [false; 24],
            days: [false; 32],
            months: [false; 13],
            weekdays: [false; 7],
            days_restricted: *day != "*",
            weekdays_restricted: *weekday != "*",
        };
        field(minute, 0, 59, "minute", &mut s.minutes)?;
        field(hour, 0, 23, "hour", &mut s.hours)?;
        field(day, 1, 31, "day of month", &mut s.days)?;
        field(month, 1, 12, "month", &mut s.months)?;
        // 7 is Sunday too
        let mut weekdays = [false; 8];
        field(weekday, 0, 7, "day of week", &mut weekdays)?;
        for (d, on) in weekdays.iter().enumerate() {
            if *on {
                s.weekdays[d % 7] = true;
            }
        }
        Ok(s)
    }

    fn day_matches(&self, t: &DateTime<Utc>) -> bool {
        let day = self.days[t.day() as usize];
        let weekday = self.weekdays[t.weekday().num_days_from_sunday() as usize];
        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => day || weekday,
            (true, false) => day,
            (false, true) => weekday,
            (false, false) => true,
        }
    }

    /// The first slot strictly after `t`, if there is one within five years.
    pub fn next_after(&self, t: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut t = t.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let limit = t + Duration::days(5 * 366);
        while t <= limit {
            if !self.months[t.month() as usize] {
                // First minute of the next month
                let (y, m) = if t.month() == 12 {
                    (t.year() + 1, 1)
                } else {
                    (t.year(), t.month() + 1)
                };
                t = Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).single()?;
                continue;
            }
            if !self.day_matches(&t) {
                t = (t + Duration::days(1)).with_hour(0)?.with_minute(0)?;
                continue;
            }
            if !self.hours[t.hour() as usize] {
                t = (t + Duration::hours(1)).with_minute(0)?;
                continue;
            }
            if !self.minutes[t.minute() as usize] {
                t += Duration::minutes(1);
                continue;
            }
            return Some(t);
        }
        None
    }
}

/// Marks the values of one field in `out` (indexed by value).
fn field(text: &str, min: u32, max: u32, what: &str, out: &mut [bool]) -> Result<(), String> {
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .ok()
                    .filter(|s| *s > 0)
                    .ok_or_else(|| format!("{what}: `{part}` has an invalid step"))?,
            ),
            None => (part, 1),
        };
        let number = |v: &str| {
            v.parse::<u32>()
                .ok()
                .filter(|n| (min..=max).contains(n))
                .ok_or_else(|| format!("{what}: `{v}` is not between {min} and {max}"))
        };
        let (lo, hi) = match range {
            "*" => (min, max),
            r => match r.split_once('-') {
                Some((a, b)) => (number(a)?, number(b)?),
                None if step > 1 => (number(r)?, max),
                None => {
                    let n = number(r)?;
                    (n, n)
                }
            },
        };
        if lo > hi {
            return Err(format!("{what}: `{range}` is an empty range"));
        }
        let mut v = lo;
        while v <= hi {
            out[v as usize] = true;
            v += step;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn next(schedule: &str, after: &str) -> String {
        Schedule::parse(schedule)
            .unwrap()
            .next_after(at(after))
            .unwrap()
            .to_rfc3339()
    }

    #[test]
    fn next_slots() {
        assert_eq!(
            next("* * * * *", "2026-10-13T10:15:30Z"),
            "2026-10-13T10:16:00+00:00"
        );
        assert_eq!(
            next("*/15 * * * *", "2026-10-13T10:15:00Z"),
            "2026-10-13T10:30:00+00:00"
        );
        assert_eq!(
            next("0 3 * * *", "2026-10-13T10:15:00Z"),
            "2026-10-14T03:00:00+00:00"
        );
        assert_eq!(
            next("@hourly", "2026-10-13T23:59:00Z"),
            "2026-10-14T00:00:00+00:00"
        );
        assert_eq!(
            next("@monthly", "2026-12-31T12:00:00Z"),
            "2027-01-01T00:00:00+00:00"
        );
        assert_eq!(
            next("30 9 * * 1-5", "2026-10-16T10:00:00Z"),
            "2026-10-19T09:30:00+00:00"
        ); // Fri -> Mon
        assert_eq!(
            next("0 0 29 2 *", "2026-03-01T00:00:00Z"),
            "2028-02-29T00:00:00+00:00"
        );
        assert_eq!(
            next("0 12 * * 7", "2026-10-13T00:00:00Z"),
            "2026-10-18T12:00:00+00:00"
        ); // 7 = Sunday
        assert_eq!(
            next("5,35 8-9 * * *", "2026-10-13T08:35:00Z"),
            "2026-10-13T09:05:00+00:00"
        );
        assert_eq!(
            next("10-50/20 * * * *", "2026-10-13T08:30:00Z"),
            "2026-10-13T08:50:00+00:00"
        );
    }

    #[test]
    fn day_of_month_or_day_of_week() {
        // The 1st, or any Monday
        let s = "0 0 1 * 1";
        assert_eq!(next(s, "2026-10-13T00:00:00Z"), "2026-10-19T00:00:00+00:00");
        assert_eq!(next(s, "2026-10-26T00:00:00Z"), "2026-11-01T00:00:00+00:00");
    }

    #[test]
    fn errors() {
        for bad in [
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "*/0 * * * *",
            "5-1 * * * *",
            "x * * * *",
        ] {
            assert!(Schedule::parse(bad).is_err(), "{bad}");
        }
        assert!(Schedule::parse("0 0 31 2 *")
            .unwrap()
            .next_after(at("2026-01-01T00:00:00Z"))
            .is_none());
    }
}
