//! Scheduler on croner: the cron expression and the time zone come from `[schedule]`.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;
use std::str::FromStr;

#[cfg_attr(test, derive(Debug))]
pub struct Scheduler {
    cron: Cron,
    tz: Tz,
}

impl Scheduler {
    pub fn new(expression: &str, timezone: &str) -> Result<Self> {
        let cron = Cron::from_str(expression)
            .map_err(|e| anyhow!("invalid cron expression {expression:?}: {e}"))?;
        let tz: Tz = timezone.parse().map_err(|_| {
            anyhow!("unknown time zone {timezone:?}, an IANA name like America/New_York is needed")
        })?;
        Ok(Self { cron, tz })
    }

    /// The next trigger strictly after `after`.
    ///
    /// We calculate in the time zone from the config, or "0 9 * * *" fires at 09:00 UTC.
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
        let local = after.with_timezone(&self.tz);
        let next = self
            .cron
            .find_next_occurrence(&local, false)
            .context("could not calculate the next cron trigger")?;
        Ok(next.with_timezone(&Utc))
    }

    /// Whether a slot was missed between `last_run` and `now` — the computer was
    /// asleep or the daemon was off. Used for `catch_up`.
    pub fn missed_since(
        &self,
        last_run: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let Some(last_run) = last_run else {
            // First start: nothing to catch up, wait for the normal slot.
            return Ok(false);
        };
        Ok(self.next_after(last_run)? <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn next_occurrence_is_computed_in_the_configured_timezone() {
        let scheduler = Scheduler::new("0 9 * * *", "America/New_York").unwrap();
        // 09:00 in New York in summer is 13:00 UTC, not 09:00 UTC.
        assert_eq!(
            scheduler.next_after(utc("2026-09-15T12:00:00Z")).unwrap(),
            utc("2026-09-15T13:00:00Z")
        );
    }

    #[test]
    fn dst_shift_moves_the_slot_in_utc_but_not_in_local_time() {
        let scheduler = Scheduler::new("0 9 * * *", "America/New_York").unwrap();
        // On 1 November 2026 New York moves to winter time: the slot stays at
        // 09:00 local time, but moves by one hour in UTC.
        let before = scheduler.next_after(utc("2026-10-30T20:00:00Z")).unwrap();
        let after = scheduler.next_after(utc("2026-11-03T20:00:00Z")).unwrap();

        assert_eq!(before, utc("2026-10-31T13:00:00Z"));
        assert_eq!(after, utc("2026-11-04T14:00:00Z"));
    }

    #[test]
    fn first_start_has_nothing_to_catch_up() {
        let scheduler = Scheduler::new("0 9 * * *", "UTC").unwrap();
        assert!(!scheduler
            .missed_since(None, utc("2026-09-15T12:00:00Z"))
            .unwrap());
    }

    #[test]
    fn a_slot_between_last_run_and_now_counts_as_missed() {
        let scheduler = Scheduler::new("0 9 * * *", "UTC").unwrap();
        let last_run = utc("2026-09-13T09:00:00Z");

        assert!(scheduler
            .missed_since(Some(last_run), utc("2026-09-15T12:00:00Z"))
            .unwrap());
        // Same day before the slot: nothing was missed.
        assert!(!scheduler
            .missed_since(Some(last_run), utc("2026-09-13T20:00:00Z"))
            .unwrap());
    }

    #[test]
    fn bad_expression_and_timezone_are_rejected_with_a_readable_error() {
        let error = Scheduler::new("every Tuesday", "UTC")
            .unwrap_err()
            .to_string();
        assert!(error.contains("cron"), "{error}");

        let error = Scheduler::new("0 9 * * *", "Moscow")
            .unwrap_err()
            .to_string();
        assert!(error.contains("time zone"), "{error}");
    }
}
