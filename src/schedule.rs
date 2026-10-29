//! Планировщик на croner: cron-выражение и таймзона берутся из `[schedule]`.

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
            .map_err(|e| anyhow!("некорректное cron-выражение {expression:?}: {e}"))?;
        let tz: Tz = timezone.parse().map_err(|_| {
            anyhow!("неизвестная таймзона {timezone:?}, нужно IANA-имя вида America/New_York")
        })?;
        Ok(Self { cron, tz })
    }

    /// Ближайшее срабатывание строго после `after`.
    ///
    /// Считаем в таймзоне из конфига, иначе "0 9 * * *" сработает в 09:00 UTC.
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
        let local = after.with_timezone(&self.tz);
        let next = self
            .cron
            .find_next_occurrence(&local, false)
            .context("не удалось вычислить следующее срабатывание cron")?;
        Ok(next.with_timezone(&Utc))
    }

    /// Был ли пропущен слот между `last_run` и `now` — компьютер спал или демон
    /// был выключен. Используется для `catch_up`.
    pub fn missed_since(
        &self,
        last_run: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let Some(last_run) = last_run else {
            // Первый запуск: догонять нечего, ждём штатного слота.
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
        // 09:00 в Нью-Йорке летом — это 13:00 UTC, а не 09:00 UTC.
        assert_eq!(
            scheduler.next_after(utc("2026-09-15T12:00:00Z")).unwrap(),
            utc("2026-09-15T13:00:00Z")
        );
    }

    #[test]
    fn dst_shift_moves_the_slot_in_utc_but_not_in_local_time() {
        let scheduler = Scheduler::new("0 9 * * *", "America/New_York").unwrap();
        // 1 ноября 2026 Нью-Йорк переходит на зимнее время: слот остаётся в
        // 09:00 по месту, но в UTC уезжает на час.
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
        // Тот же день до слота: пропущенного нет.
        assert!(!scheduler
            .missed_since(Some(last_run), utc("2026-09-13T20:00:00Z"))
            .unwrap());
    }

    #[test]
    fn bad_expression_and_timezone_are_rejected_with_a_readable_error() {
        let error = Scheduler::new("каждый вторник", "UTC")
            .unwrap_err()
            .to_string();
        assert!(error.contains("cron"), "{error}");

        let error = Scheduler::new("0 9 * * *", "Moscow")
            .unwrap_err()
            .to_string();
        assert!(error.contains("таймзона"), "{error}");
    }
}
