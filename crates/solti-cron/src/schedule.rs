use croner::{
    Cron, JobType,
    parser::{CronParser, Seconds, Year},
};

use crate::{CronError, DateTime, Tz, Utc};

/// Immutable cron expression evaluated in one explicit IANA timezone.
///
/// Five fields mean `minute hour day-of-month month day-of-week`.
/// Six fields add seconds at the beginning.
/// Numeric weekdays use Sunday = 0 (or 7).
/// Restricted day-of-month and day-of-week fields use cron's OR semantics.
/// Lists, ranges, steps, month/weekday names, `L`, `W`, and `#` follow Croner.
/// Year fields and nicknames are rejected to keep field interpretation explicit.
///
/// Fixed-time jobs falling in a DST gap run at the first valid instant after the
/// gap when it remains on the scheduled local date; in a repeated hour they use
/// its first occurrence. Wildcard/interval jobs skip missing local times and can
/// run in both passes through a repeated hour.
#[derive(Clone, Debug)]
pub struct CronSchedule {
    expression: String,
    timezone: Tz,
    cron: Cron,
}

impl CronSchedule {
    /// Parses an expression and an IANA timezone such as `UTC` or `Europe/Berlin`.
    ///
    /// # Errors
    /// Returns [`CronError::InvalidExpression`] for invalid or unsupported syntax
    /// and [`CronError::InvalidTimezone`] for unknown timezone identifiers.
    /// A syntactically valid expression without a real calendar match is reported
    /// by [`Self::next_after`].
    pub fn parse(expression: &str, timezone: &str) -> Result<Self, CronError> {
        let expression = expression.trim();
        if !matches!(expression.split_whitespace().count(), 5 | 6) {
            return Err(CronError::InvalidExpression(
                "expected five fields, or six fields with seconds".into(),
            ));
        }
        let timezone = timezone
            .parse()
            .map_err(|_| CronError::InvalidTimezone(timezone.into()))?;
        let cron = CronParser::builder()
            .seconds(Seconds::Optional)
            .year(Year::Disallowed)
            .build()
            .parse(expression)
            .map_err(|error| CronError::InvalidExpression(error.to_string()))?;
        Ok(Self {
            expression: expression.into(),
            timezone,
            cron,
        })
    }

    /// Original expression with leading and trailing whitespace removed.
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// Timezone used for all calendar evaluation.
    #[must_use]
    pub const fn timezone(&self) -> Tz {
        self.timezone
    }

    /// Finds the first occurrence strictly after a UTC instant.
    ///
    /// The result is expressed in UTC and always has zero fractional seconds.
    ///
    /// # Errors
    /// Returns [`CronError::NoOccurrence`] when the bounded calendar search fails
    /// (years 1 through 5000). Gaps crossing a local date boundary retain
    /// Croner's search-limit behavior and may return this error.
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>, CronError> {
        let local_after = after.with_timezone(&self.timezone);
        match self.cron.find_next_occurrence(&local_after, false) {
            Ok(next) => Ok(next.with_timezone(&Utc)),
            Err(error)
                if matches!(error, croner::errors::CronError::TimeSearchLimitExceeded)
                    && self.cron.determine_job_type() == JobType::FixedTime =>
            {
                // Croner 4 caps gap resolution at two hours. Resolve a longer
                // same-date gap from the timezone table instead of scanning it.
                // UTC evaluation supplies the next nominal local calendar match;
                // GapInfo proves that match falls in an actual timezone gap.
                let nominal = self
                    .cron
                    .find_next_occurrence(&local_after.naive_local().and_utc(), false)
                    .ok();
                let gap_end = nominal.and_then(|nominal| {
                    chrono_tz::GapInfo::new(&nominal.naive_utc(), &self.timezone)
                        .and_then(|gap| gap.end)
                        .filter(|end| {
                            end.date_naive() == nominal.date_naive() && *end > local_after
                        })
                });
                gap_end
                    .map(|end| end.with_timezone(&Utc))
                    .ok_or(CronError::NoOccurrence(error))
            }
            Err(error) => Err(CronError::NoOccurrence(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn five_and_six_fields_have_explicit_seconds() {
        let after = utc("2026-01-01T12:00:00Z");
        assert_eq!(
            CronSchedule::parse("*/5 * * * *", "UTC")
                .unwrap()
                .next_after(after)
                .unwrap(),
            utc("2026-01-01T12:05:00Z")
        );
        assert_eq!(
            CronSchedule::parse("*/5 * * * * *", "UTC")
                .unwrap()
                .next_after(after)
                .unwrap(),
            utc("2026-01-01T12:00:05Z")
        );
        assert_eq!(
            CronSchedule::parse("* * * * * *", "UTC")
                .unwrap()
                .next_after(utc("2026-01-01T12:00:00.500Z"))
                .unwrap(),
            utc("2026-01-01T12:00:01Z")
        );
    }

    #[test]
    fn named_timezone_and_weekday_calendar_are_honored() {
        let schedule = CronSchedule::parse("0 9 * * MON-FRI", "Asia/Tbilisi").unwrap();
        assert_eq!(schedule.timezone(), "Asia/Tbilisi".parse::<Tz>().unwrap());
        assert_eq!(
            schedule.next_after(utc("2026-01-02T05:00:00Z")).unwrap(),
            utc("2026-01-05T05:00:00Z")
        );
    }

    #[test]
    fn leap_days_and_day_or_semantics_are_honored() {
        assert_eq!(
            CronSchedule::parse("0 0 29 FEB *", "UTC")
                .unwrap()
                .next_after(utc("2025-03-01T00:00:00Z"))
                .unwrap(),
            utc("2028-02-29T00:00:00Z")
        );
        assert_eq!(
            CronSchedule::parse("0 0 13 * MON", "UTC")
                .unwrap()
                .next_after(utc("2026-01-11T00:00:00Z"))
                .unwrap(),
            utc("2026-01-12T00:00:00Z")
        );
    }

    #[test]
    fn dst_fixed_time_gap_and_overlap_have_stable_utc_results() {
        let schedule = CronSchedule::parse("30 2 * * *", "Europe/Berlin").unwrap();
        assert_eq!(
            schedule.next_after(utc("2026-03-29T00:59:59Z")).unwrap(),
            utc("2026-03-29T01:00:00Z")
        );
        let first = schedule.next_after(utc("2026-10-25T00:00:00Z")).unwrap();
        assert_eq!(first, utc("2026-10-25T00:30:00Z"));
        assert_eq!(
            schedule.next_after(first).unwrap(),
            utc("2026-10-26T01:30:00Z")
        );
    }

    #[test]
    fn fixed_time_in_three_hour_gap_uses_first_valid_instant() {
        let schedule = CronSchedule::parse("2 0 * * *", "Antarctica/Casey").unwrap();
        let end = utc("2020-10-03T16:01:00Z");
        assert_eq!(
            schedule.next_after(utc("2020-10-03T16:00:00Z")).unwrap(),
            end
        );
        assert_eq!(
            schedule
                .next_after(utc("2020-10-03T16:00:59.500Z"))
                .unwrap(),
            end
        );
        assert_eq!(
            schedule.next_after(end).unwrap(),
            utc("2020-10-04T13:02:00Z")
        );
    }

    #[test]
    fn dst_interval_jobs_follow_both_real_passes() {
        let schedule = CronSchedule::parse("*/15 2 * * *", "Europe/Berlin").unwrap();
        assert_eq!(
            schedule.next_after(utc("2026-10-25T00:45:00Z")).unwrap(),
            utc("2026-10-25T01:00:00Z")
        );
    }

    #[test]
    fn invalid_syntax_timezone_and_impossible_calendar_are_errors() {
        for expression in [
            "",
            "@daily",
            "* * * *",
            "* * * * * * 2026",
            "60 * * * *",
            "*/0 * * * *",
        ] {
            assert!(
                matches!(
                    CronSchedule::parse(expression, "UTC"),
                    Err(CronError::InvalidExpression(_))
                ),
                "{expression}"
            );
        }
        assert!(matches!(
            CronSchedule::parse("* * * * *", "Mars/Olympus"),
            Err(CronError::InvalidTimezone(_))
        ));
        assert!(matches!(
            CronSchedule::parse("0 0 30 FEB *", "UTC")
                .unwrap()
                .next_after(utc("2026-01-01T00:00:00Z")),
            Err(CronError::NoOccurrence(_))
        ));
    }
}
