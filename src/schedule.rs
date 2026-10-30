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
