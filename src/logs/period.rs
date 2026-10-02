//! Calendar periods and intervals used by the leaderboard and activity endpoints.
//!
//! All periods are calendar periods in UTC: a week starts on Monday, a month on the first day
//! of the month. The ClickHouse server is expected to run in UTC (the default of the official
//! image), like it is for the rest of rustlog.

use chrono::{DateTime, Datelike, Days, Months, NaiveDate, NaiveTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The longest range activity can be requested for
pub const MAX_ACTIVITY_DAYS: i64 = 3660;

/// A calendar period
#[derive(Deserialize, Serialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    #[default]
    Day,
    Week,
    Month,
    Year,
    All,
}

/// The size of the buckets activity is grouped into
#[derive(Deserialize, Serialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Interval {
    #[default]
    Day,
    Week,
    Month,
    Year,
}

/// A range of days. The start is inclusive, the end is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateRange {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl DateRange {
    /// A range which covers all days that can have logs
    pub fn all_time() -> Self {
        Self {
            start: NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid date"),
            end: NaiveDate::from_ymd_opt(2100, 1, 1).expect("valid date"),
        }
    }

    pub fn days(&self) -> i64 {
        (self.end - self.start).num_days()
    }

    /// Midnight at the start of the first day
    pub fn start_time(&self) -> DateTime<Utc> {
        start_of_day(self.start)
    }

    /// Midnight at the end of the last day
    pub fn end_time(&self) -> DateTime<Utc> {
        start_of_day(self.end)
    }
}

impl Period {
    /// The days of the period which contains `date`, `None` for `All`
    pub fn range_containing(self, date: NaiveDate) -> Option<DateRange> {
        let interval = match self {
            Period::Day => Interval::Day,
            Period::Week => Interval::Week,
            Period::Month => Interval::Month,
            Period::Year => Interval::Year,
            Period::All => return None,
        };

        let start = interval.bucket_start(date)?;
        Some(DateRange {
            start,
            end: interval.next_bucket_start(start)?,
        })
    }

    /// The days to query: the period containing `date`, or all days for `Period::All`
    pub fn range_or_all(self, date: NaiveDate) -> Option<DateRange> {
        match self {
            Period::All => Some(DateRange::all_time()),
            _ => self.range_containing(date),
        }
    }
}

impl Interval {
    /// The first day of the bucket which contains `date`
    pub fn bucket_start(self, date: NaiveDate) -> Option<NaiveDate> {
        match self {
            Interval::Day => Some(date),
            // weeks start on Monday
            Interval::Week => {
                date.checked_sub_days(Days::new(date.weekday().num_days_from_monday().into()))
            }
            Interval::Month => date.with_day(1),
            Interval::Year => NaiveDate::from_ymd_opt(date.year(), 1, 1),
        }
    }

    /// The first day of the bucket after the one starting at `start`
    pub fn next_bucket_start(self, start: NaiveDate) -> Option<NaiveDate> {
        match self {
            Interval::Day => start.checked_add_days(Days::new(1)),
            Interval::Week => start.checked_add_days(Days::new(7)),
            Interval::Month => start.checked_add_months(Months::new(1)),
            Interval::Year => NaiveDate::from_ymd_opt(start.year() + 1, 1, 1),
        }
    }

    /// The first days of all buckets which overlap a range, the first one may start before it
    pub fn bucket_starts(self, range: DateRange) -> Vec<NaiveDate> {
        let mut starts = Vec::new();

        let mut current = match self.bucket_start(range.start) {
            Some(start) => start,
            None => return starts,
        };
        while current < range.end {
            starts.push(current);
            current = match self.next_bucket_start(current) {
                Some(next) => next,
                None => break,
            };
        }

        starts
    }

    /// How many days are shown by default
    fn default_span_days(self) -> u64 {
        match self {
            Interval::Day => 30,
            Interval::Week => 26 * 7,
            Interval::Month => 365,
            Interval::Year => 5 * 365,
        }
    }
}

/// Midnight (UTC) at the start of a day
pub fn start_of_day(date: NaiveDate) -> DateTime<Utc> {
    date.and_time(NaiveTime::default()).and_utc()
}

/// Dates outside of these years cannot have logs, and are not worth passing to the database
fn is_plausible(date: &NaiveDate) -> bool {
    (1970..2100).contains(&date.year())
}

/// Parses `YYYY-MM-DD`, `YYYY-MM` (first day of the month) or `YYYY` (first day of the year)
pub fn parse_date(text: &str) -> Option<NaiveDate> {
    let text = text.trim();

    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .or_else(|| NaiveDate::parse_from_str(&format!("{text}-01"), "%Y-%m-%d").ok())
        .or_else(|| NaiveDate::parse_from_str(&format!("{text}-01-01"), "%Y-%m-%d").ok())
        .filter(is_plausible)
}

/// Parses a date, or an RFC 3339 timestamp of which only the (UTC) date is used
pub fn parse_date_or_timestamp(text: &str) -> Option<NaiveDate> {
    match DateTime::parse_from_rfc3339(text.trim()) {
        Ok(timestamp) => Some(timestamp.with_timezone(&Utc).date_naive()).filter(is_plausible),
        Err(_) => parse_date(text),
    }
}

/// Parses a point in time: an RFC 3339 timestamp, or a date which means the start of that day
/// (UTC)
pub fn parse_instant(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();

    match DateTime::parse_from_rfc3339(text) {
        Ok(timestamp) => Some(timestamp.with_timezone(&Utc))
            .filter(|timestamp| is_plausible(&timestamp.date_naive())),
        Err(_) => parse_date(text).map(start_of_day),
    }
}

/// The days activity is returned for. Both `from` and `to` are inclusive days, the defaults are
/// the last days up to and including `today`.
///
/// The range is widened to whole intervals: the first day is moved back to the start of its
/// interval and the end is the start of the interval after the one with the last day. This way
/// every bucket counts all of its days, a weekly bucket is never a partial week.
pub fn activity_range(
    interval: Interval,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    today: NaiveDate,
) -> Result<DateRange, &'static str> {
    const OUT_OF_RANGE: &str = "Date out of range";

    let last_day = to.unwrap_or(today);
    let first_day = match from {
        Some(from) => from,
        None => last_day
            .checked_sub_days(Days::new(interval.default_span_days() - 1))
            .ok_or(OUT_OF_RANGE)?,
    };

    if first_day > last_day {
        return Err("`from` must not be after `to`");
    }

    let start = interval.bucket_start(first_day).ok_or(OUT_OF_RANGE)?;
    let end = interval
        .bucket_start(last_day)
        .and_then(|last_bucket| interval.next_bucket_start(last_bucket))
        .ok_or(OUT_OF_RANGE)?;

    let range = DateRange { start, end };
    if range.days() > MAX_ACTIVITY_DAYS {
        return Err("The requested range is too long");
    }

    Ok(range)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn range(start: NaiveDate, end: NaiveDate) -> Option<DateRange> {
        Some(DateRange { start, end })
    }

    #[test]
    fn day_period() {
        assert_eq!(
            Period::Day.range_containing(date(2026, 10, 2)),
            range(date(2026, 10, 2), date(2026, 10, 3))
        );
        assert_eq!(
            Period::Day.range_containing(date(2026, 12, 31)),
            range(date(2026, 12, 31), date(2027, 1, 1))
        );
    }

    #[test]
    fn weeks_start_on_monday() {
        // 2026-10-02 is a Friday
        assert_eq!(
            Period::Week.range_containing(date(2026, 10, 2)),
            range(date(2026, 9, 28), date(2026, 10, 5))
        );
        // a Monday is the first day of its own week
        assert_eq!(
            Period::Week.range_containing(date(2026, 9, 28)),
            range(date(2026, 9, 28), date(2026, 10, 5))
        );
        // a Sunday is the last day of its week
        assert_eq!(
            Period::Week.range_containing(date(2026, 10, 4)),
            range(date(2026, 9, 28), date(2026, 10, 5))
        );
        // across a year boundary
        assert_eq!(
            Period::Week.range_containing(date(2026, 1, 1)),
            range(date(2025, 12, 29), date(2026, 1, 5))
        );
    }

    #[test]
    fn month_period() {
        assert_eq!(
            Period::Month.range_containing(date(2026, 10, 17)),
            range(date(2026, 10, 1), date(2026, 11, 1))
        );
        assert_eq!(
            Period::Month.range_containing(date(2026, 12, 31)),
            range(date(2026, 12, 1), date(2027, 1, 1))
        );
        // leap year february
        assert_eq!(
            Period::Month.range_containing(date(2028, 2, 10)),
            range(date(2028, 2, 1), date(2028, 3, 1))
        );
    }

    #[test]
    fn year_period() {
        assert_eq!(
            Period::Year.range_containing(date(2026, 10, 2)),
            range(date(2026, 1, 1), date(2027, 1, 1))
        );
    }

    #[test]
    fn all_time_has_no_calendar_range() {
        assert_eq!(Period::All.range_containing(date(2026, 10, 2)), None);
        assert_eq!(
            Period::All.range_or_all(date(2026, 10, 2)),
            Some(DateRange::all_time())
        );
        assert_eq!(
            Period::Month.range_or_all(date(2026, 10, 2)),
            Period::Month.range_containing(date(2026, 10, 2))
        );
    }

    #[test]
    fn range_helpers() {
        let range = DateRange {
            start: date(2026, 10, 1),
            end: date(2026, 11, 1),
        };
        assert_eq!(range.days(), 31);
        assert_eq!(range.start_time().to_rfc3339(), "2026-10-01T00:00:00+00:00");
        assert_eq!(range.end_time().to_rfc3339(), "2026-11-01T00:00:00+00:00");
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_date("2026-09-15"), Some(date(2026, 9, 15)));
        assert_eq!(parse_date(" 2026-09 "), Some(date(2026, 9, 1)));
        assert_eq!(parse_date("2026"), Some(date(2026, 1, 1)));
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("2026-02-30"), None);
        assert_eq!(parse_date("yesterday"), None);
        assert_eq!(parse_date(""), None);
        // implausible years are rejected
        assert_eq!(parse_date("0026-09-01"), None);
        assert_eq!(parse_date("3000"), None);
    }

    #[test]
    fn parses_timestamps_to_their_utc_date() {
        assert_eq!(
            parse_date_or_timestamp("2026-09-15T23:30:00Z"),
            Some(date(2026, 9, 15))
        );
        // 23:30 at UTC-2 is already the next day in UTC
        assert_eq!(
            parse_date_or_timestamp("2026-09-15T23:30:00-02:00"),
            Some(date(2026, 9, 16))
        );
        assert_eq!(parse_date_or_timestamp("2026-09-15"), Some(date(2026, 9, 15)));
        assert_eq!(parse_date_or_timestamp("nonsense"), None);
    }

    #[test]
    fn parses_points_in_time() {
        // a timestamp keeps its time, in UTC
        assert_eq!(
            parse_instant("2026-09-15T23:30:15+02:00").map(|time| time.to_rfc3339()),
            Some("2026-09-15T21:30:15+00:00".to_owned())
        );
        // a date is the start of the day
        assert_eq!(
            parse_instant(" 2026-09-15 ").map(|time| time.to_rfc3339()),
            Some("2026-09-15T00:00:00+00:00".to_owned())
        );
        assert_eq!(parse_instant("yesterday"), None);
        assert_eq!(parse_instant("0001-01-01T00:00:00Z"), None);
        assert_eq!(parse_instant("2100-01-01T00:00:00Z"), None);
        assert_eq!(parse_instant(""), None);
    }

    #[test]
    fn implausible_timestamps_are_rejected() {
        assert_eq!(
            parse_date_or_timestamp("1970-01-01T00:00:00Z"),
            Some(date(1970, 1, 1))
        );
        assert_eq!(parse_date_or_timestamp("1969-12-31T23:59:59Z"), None);
        // 00:30 at UTC+1 is still the day before in UTC
        assert_eq!(parse_date_or_timestamp("1970-01-01T00:30:00+01:00"), None);
        assert_eq!(parse_date_or_timestamp("0001-01-01T00:00:00Z"), None);
        assert_eq!(parse_date_or_timestamp("2099-12-31T23:59:59Z"), Some(date(2099, 12, 31)));
        assert_eq!(parse_date_or_timestamp("2100-01-01T00:00:00Z"), None);
    }

    #[test]
    fn activity_range_defaults_to_the_latest_days() {
        let today = date(2026, 10, 2);

        assert_eq!(
            activity_range(Interval::Day, None, None, today),
            Ok(DateRange {
                start: date(2026, 9, 3),
                end: date(2026, 10, 3)
            })
        );

        // Whole intervals: 26 weeks back from 2026-10-02 is the Saturday 2026-04-04, which is in
        // the week starting on Monday 2026-03-30. The current week ends with Sunday 2026-10-04.
        let weeks = activity_range(Interval::Week, None, None, today).unwrap();
        assert_eq!(weeks.start, date(2026, 3, 30));
        assert_eq!(weeks.end, date(2026, 10, 5));
        assert_eq!(weeks.days(), 189);

        assert_eq!(
            activity_range(Interval::Month, None, None, today),
            Ok(DateRange {
                start: date(2025, 10, 1),
                end: date(2026, 11, 1)
            })
        );
        assert_eq!(
            activity_range(Interval::Year, None, None, today),
            Ok(DateRange {
                start: date(2021, 1, 1),
                end: date(2027, 1, 1)
            })
        );
    }

    #[test]
    fn activity_range_covers_whole_intervals() {
        let today = date(2026, 10, 2);

        // Wednesday to Wednesday: both partial weeks are completed
        assert_eq!(
            activity_range(Interval::Week, Some(date(2026, 9, 30)), Some(date(2026, 10, 7)), today),
            Ok(DateRange {
                start: date(2026, 9, 28),
                end: date(2026, 10, 12)
            })
        );
        // a single Sunday is its whole week
        assert_eq!(
            activity_range(Interval::Week, Some(date(2026, 10, 4)), Some(date(2026, 10, 4)), today),
            Ok(DateRange {
                start: date(2026, 9, 28),
                end: date(2026, 10, 5)
            })
        );
        assert_eq!(
            activity_range(Interval::Year, Some(date(2025, 6, 15)), Some(date(2026, 2, 1)), today),
            Ok(DateRange {
                start: date(2025, 1, 1),
                end: date(2027, 1, 1)
            })
        );
    }

    #[test]
    fn activity_range_includes_both_ends() {
        let today = date(2026, 10, 2);

        assert_eq!(
            activity_range(
                Interval::Day,
                Some(date(2026, 9, 1)),
                Some(date(2026, 9, 30)),
                today
            ),
            Ok(DateRange {
                start: date(2026, 9, 1),
                end: date(2026, 10, 1)
            })
        );
        // only `from`: up to today, which is completed to the end of its month
        assert_eq!(
            activity_range(Interval::Month, Some(date(2026, 10, 1)), None, today),
            Ok(DateRange {
                start: date(2026, 10, 1),
                end: date(2026, 11, 1)
            })
        );
        assert_eq!(
            activity_range(Interval::Day, Some(date(2026, 10, 1)), None, today),
            Ok(DateRange {
                start: date(2026, 10, 1),
                end: date(2026, 10, 3)
            })
        );
    }

    #[test]
    fn activity_range_rejects_invalid_ranges() {
        let today = date(2026, 10, 2);

        assert!(activity_range(
            Interval::Day,
            Some(date(2026, 10, 2)),
            Some(date(2026, 10, 1)),
            today
        )
        .is_err());
        assert!(activity_range(Interval::Day, Some(date(2000, 1, 1)), None, today).is_err());
    }

    #[test]
    fn the_length_limit_applies_to_the_widened_range() {
        let today = date(2026, 10, 2);

        // 3654 days from a Monday up to the end of the current week
        assert!(activity_range(Interval::Week, Some(date(2016, 10, 3)), None, today).is_ok());
        // a Sunday, which makes it start in the week before: 3661 days, one too many
        assert_eq!(
            activity_range(Interval::Week, Some(date(2016, 10, 2)), None, today),
            Err("The requested range is too long")
        );
    }

    #[test]
    fn bucket_starts_cover_the_range() {
        let range = DateRange {
            start: date(2026, 9, 30),
            end: date(2026, 10, 3),
        };

        assert_eq!(
            Interval::Day.bucket_starts(range),
            vec![date(2026, 9, 30), date(2026, 10, 1), date(2026, 10, 2)]
        );
        // 2026-09-30 is a Wednesday, its week starts on 2026-09-28
        assert_eq!(Interval::Week.bucket_starts(range), vec![date(2026, 9, 28)]);
        assert_eq!(
            Interval::Month.bucket_starts(range),
            vec![date(2026, 9, 1), date(2026, 10, 1)]
        );
        assert_eq!(Interval::Year.bucket_starts(range), vec![date(2026, 1, 1)]);
    }

    #[test]
    fn bucket_starts_of_longer_ranges() {
        let range = DateRange {
            start: date(2025, 11, 15),
            end: date(2026, 2, 2),
        };

        assert_eq!(
            Interval::Month.bucket_starts(range),
            vec![
                date(2025, 11, 1),
                date(2025, 12, 1),
                date(2026, 1, 1),
                date(2026, 2, 1)
            ]
        );
        assert_eq!(
            Interval::Year.bucket_starts(range),
            vec![date(2025, 1, 1), date(2026, 1, 1)]
        );
        // from the week of 2025-11-10 until the week of 2026-01-26
        let weeks = Interval::Week.bucket_starts(range);
        assert_eq!(weeks.len(), 12);
        assert_eq!(weeks.first(), Some(&date(2025, 11, 10)));
        assert_eq!(weeks.last(), Some(&date(2026, 1, 26)));
    }

    #[test]
    fn empty_ranges_have_no_buckets() {
        let range = DateRange {
            start: date(2026, 10, 2),
            end: date(2026, 10, 2),
        };
        assert_eq!(Interval::Day.bucket_starts(range), vec![]);
    }

    #[test]
    fn enums_use_lowercase_names() {
        assert_eq!(serde_json::to_string(&Period::Week).unwrap(), r#""week""#);
        assert_eq!(
            serde_json::from_str::<Period>(r#""all""#).unwrap(),
            Period::All
        );
        assert_eq!(
            serde_json::from_str::<Interval>(r#""month""#).unwrap(),
            Interval::Month
        );
        assert!(serde_json::from_str::<Interval>(r#""all""#).is_err());
        assert_eq!(Period::default(), Period::Day);
    }
}
