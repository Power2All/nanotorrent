//! When to stop seeding, and when to slow down.
//!
//! Pure decisions: every function here takes the settings and the numbers and
//! returns what should happen, touching neither the engine nor the clock. The
//! tasks that act on the answers live in [`super::session`], which is where the
//! handles are.
//!
//! Split out because this is the part worth testing. "Does a schedule that runs
//! from 22:00 to 06:00 cover 02:00 on a Tuesday" is a question with a wrong
//! answer that would otherwise only show up as somebody's connection being
//! throttled at the wrong time of day.

use chrono::{DateTime, Datelike, Local, Timelike};

use crate::core::configuration::Configuration;

/// What to do with a torrent that has met its share limit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShareAction {
    Pause,
    Remove { with_data: bool },
}

impl ShareAction {
    /// Parse the stored setting. Anything unrecognised is a pause, which is the
    /// one option that destroys nothing - a typo in the database must not turn
    /// into deleted downloads.
    fn parse(value: &str) -> ShareAction {
        match value {
            "remove" => ShareAction::Remove { with_data: false },
            "remove_with_data" => ShareAction::Remove { with_data: true },
            _ => ShareAction::Pause,
        }
    }
}

/// The share limits in force, after per-torrent overrides.
#[derive(Clone, Copy, Debug)]
pub struct ShareLimits {
    /// Stop at this upload/download ratio. `None` is no limit.
    pub ratio: Option<f64>,
    /// Stop after this many minutes of seeding. `None` is no limit.
    pub seed_minutes: Option<i64>,
    pub action: ShareAction,
}

impl ShareLimits {
    /// Read the global limits.
    ///
    /// `libtorrent.share_ratio_limit` is inherited from PicoTorrent and is
    /// reused rather than replaced - it is the same question, and two keys
    /// answering it would eventually disagree.
    ///
    /// It is stored in **hundredths**, which is libtorrent's own convention and
    /// what the migration's default of `200` means: ratio 2.00. Reading it as a
    /// plain number would compare a ratio against 200 and never fire, which is
    /// the quiet kind of wrong.
    ///
    /// Anything at or below zero is "no limit", which is the convention the
    /// inherited keys already use.
    ///
    /// Gated on `queue.share_limit_enabled`, which exists precisely because the
    /// inherited default is 200 rather than 0: without the switch, every
    /// upgraded profile would silently start stopping its torrents at ratio 2.
    /// Off returns no limits at all rather than reading the numbers, so what is
    /// in those boxes is what the switch would turn on - not what is running.
    pub fn global(cfg: &Configuration) -> ShareLimits {
        if !cfg.get_bool("queue.share_limit_enabled") {
            return ShareLimits {
                ratio: None,
                seed_minutes: None,
                action: ShareAction::Pause,
            };
        }
        ShareLimits {
            ratio: positive(cfg.get_int("libtorrent.share_ratio_limit"))
                .map(|hundredths| hundredths as f64 / 100.0),
            seed_minutes: positive(cfg.get_int("queue.seed_time_limit")),
            action: ShareAction::parse(
                &cfg.get_string("queue.share_limit_action")
                    .unwrap_or_else(|| String::from("pause")),
            ),
        }
    }

    /// Apply a torrent's own overrides. `None` in either means "follow the
    /// global setting", which is why they are nullable columns rather than
    /// defaulted ones - see the migration.
    pub fn with_overrides(self, ratio: Option<f64>, seed_minutes: Option<i64>) -> ShareLimits {
        ShareLimits {
            // Two levels of Option, and they mean different things: the outer
            // one is "did this torrent say anything", the inner is "and was it
            // a limit or an explicit none".
            ratio: match ratio {
                Some(own) => positive(Some(own)),
                None => self.ratio,
            },
            seed_minutes: match seed_minutes {
                Some(own) => positive(Some(own)),
                None => self.seed_minutes,
            },
            ..self
        }
    }

    /// Has this torrent done enough?
    ///
    /// Either limit alone is enough to stop, which is what people expect:
    /// "ratio 2 or a week, whichever comes first".
    pub fn reached(&self, ratio: f64, seeded_minutes: i64) -> bool {
        let by_ratio = self.ratio.is_some_and(|limit| ratio >= limit);
        let by_time = self
            .seed_minutes
            .is_some_and(|limit| seeded_minutes >= limit);
        by_ratio || by_time
    }
}

/// Treat zero and negatives as "no limit".
fn positive<T: PartialOrd + Default>(value: Option<T>) -> Option<T> {
    value.filter(|v| *v > T::default())
}

/// The rate limits that should be in force right now, in bytes per second.
///
/// `None` in either half means unlimited. Returned as a pair rather than
/// applied here so the caller can compare it against what is already set and
/// stay quiet when nothing has changed - this is consulted every second.
pub fn current_rates(cfg: &Configuration, now: DateTime<Local>) -> (Option<u32>, Option<u32>) {
    if alt_speed_active(cfg, now) {
        return (
            rate(cfg.get_int("speed.alt_download_rate")),
            rate(cfg.get_int("speed.alt_upload_rate")),
        );
    }
    let down = cfg
        .get_bool("libtorrent.enable_download_rate_limit")
        .then(|| rate(cfg.get_int("libtorrent.download_rate_limit")))
        .flatten();
    let up = cfg
        .get_bool("libtorrent.enable_upload_rate_limit")
        .then(|| rate(cfg.get_int("libtorrent.upload_rate_limit")))
        .flatten();
    (down, up)
}

/// Rate limits are stored in KB/s, like the original. Zero is unlimited.
fn rate(kb: Option<i64>) -> Option<u32> {
    kb.filter(|kb| *kb > 0).map(|kb| (kb * 1024) as u32)
}

/// Are the alternative limits in force?
///
/// The manual switch wins outright. The schedule only turns them *on* during
/// its window: it is a second way to reach the same switch, not a third state,
/// so someone who flips the toggle inside the window is not overruled a second
/// later by the scheduler agreeing with them.
pub fn alt_speed_active(cfg: &Configuration, now: DateTime<Local>) -> bool {
    if cfg.get_bool("speed.alt_enabled") {
        return true;
    }
    if !cfg.get_bool("speed.schedule_enabled") {
        return false;
    }
    schedule_covers(
        cfg.get_int("speed.schedule_from").unwrap_or(0),
        cfg.get_int("speed.schedule_to").unwrap_or(0),
        cfg.get_int("speed.schedule_days").unwrap_or(127),
        now,
    )
}

/// Does a schedule cover this moment?
///
/// `from` and `to` are minutes since midnight; `days` is a bitmask with Monday
/// at bit 0. A window where `to` is before `from` wraps past midnight, and the
/// day it is checked against is the day the window *started* - so "22:00 to
/// 06:00, weekdays" still applies at 02:00 on Saturday morning, because that is
/// Friday night's window. Getting that backwards is the classic bug here.
pub fn schedule_covers(from: i64, to: i64, days: i64, now: DateTime<Local>) -> bool {
    let minute = now.hour() as i64 * 60 + now.minute() as i64;
    let today = now.weekday().num_days_from_monday() as i64;
    let yesterday = (today + 6) % 7;

    let on = |day: i64| days & (1 << day) != 0;

    if from <= to {
        on(today) && minute >= from && minute < to
    } else {
        // Wrapped: the evening part belongs to today, the morning part to the
        // window that opened yesterday.
        (on(today) && minute >= from) || (on(yesterday) && minute < to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        // 2026-09-07 is a Monday, so `day` 0..6 lands on Monday..Sunday.
        Local
            .with_ymd_and_hms(2026, 9, 7 + day, hour, minute, 0)
            .unwrap()
    }

    const EVERY_DAY: i64 = 127;

    #[test]
    fn a_daytime_window_covers_only_the_daytime() {
        let (from, to) = (9 * 60, 17 * 60);
        assert!(!schedule_covers(from, to, EVERY_DAY, at(0, 8, 59)));
        assert!(schedule_covers(from, to, EVERY_DAY, at(0, 9, 0)));
        assert!(schedule_covers(from, to, EVERY_DAY, at(0, 16, 59)));
        // Exclusive at the end, so a window ending at 17:00 is off at 17:00
        // rather than running one minute long.
        assert!(!schedule_covers(from, to, EVERY_DAY, at(0, 17, 0)));
    }

    /// The one that is easy to get wrong: 22:00-06:00 has to be on at 02:00,
    /// and the day it belongs to is the evening it started.
    #[test]
    fn a_window_that_wraps_past_midnight_stays_on_until_morning() {
        let (from, to) = (22 * 60, 6 * 60);
        assert!(schedule_covers(from, to, EVERY_DAY, at(0, 23, 30)));
        assert!(schedule_covers(from, to, EVERY_DAY, at(1, 2, 0)));
        assert!(!schedule_covers(from, to, EVERY_DAY, at(1, 7, 0)));
    }

    /// Friday night's window still applies on Saturday morning, even though
    /// Saturday is not one of the selected days.
    #[test]
    fn a_wrapped_window_belongs_to_the_day_it_opened() {
        let weekdays = 0b0011111; // Monday..Friday
        let (from, to) = (22 * 60, 6 * 60);

        // Friday 23:00 - on, and Saturday 02:00 - still that same window.
        assert!(schedule_covers(from, to, weekdays, at(4, 23, 0)));
        assert!(schedule_covers(from, to, weekdays, at(5, 2, 0)));
        // But Saturday evening is not, because Saturday is not selected.
        assert!(!schedule_covers(from, to, weekdays, at(5, 23, 0)));
    }

    #[test]
    fn either_limit_is_enough_to_stop() {
        let limits = ShareLimits {
            ratio: Some(2.0),
            seed_minutes: Some(60),
            action: ShareAction::Pause,
        };
        assert!(!limits.reached(1.9, 59));
        assert!(limits.reached(2.0, 0), "the ratio limit did not stop it");
        assert!(limits.reached(0.0, 60), "the time limit did not stop it");
    }

    /// The stored ratio is in hundredths. Reading it as a plain number would
    /// mean comparing a ratio of 2.0 against a limit of 200, so the limit would
    /// never be reached and nothing would ever stop seeding.
    #[test]
    fn the_stored_ratio_is_hundredths() {
        let db = std::sync::Arc::new(crate::core::database::Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        let cfg = Configuration::new(db);
        cfg.set("queue.share_limit_enabled", &true);

        // The migration's own default, which means ratio 2.00.
        cfg.set("libtorrent.share_ratio_limit", &200i64);
        let limits = ShareLimits::global(&cfg);
        assert_eq!(limits.ratio, Some(2.0));
        assert!(limits.reached(2.0, 0));
        assert!(!limits.reached(1.99, 0));

        cfg.set("libtorrent.share_ratio_limit", &0i64);
        assert_eq!(ShareLimits::global(&cfg).ratio, None);
    }

    /// The inherited default is 200 - ratio 2.00 - and nothing read it before.
    /// An upgraded profile must therefore come up with the limits OFF, or every
    /// torrent silently stops seeding at ratio 2 the first time this build runs.
    #[test]
    fn share_limits_are_off_until_switched_on() {
        let db = std::sync::Arc::new(crate::core::database::Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        let cfg = Configuration::new(db);

        // Exactly what an upgraded profile looks like: the inherited value
        // present, and nobody having asked for limits.
        assert_eq!(cfg.get_int("libtorrent.share_ratio_limit"), Some(200));

        let limits = ShareLimits::global(&cfg);
        assert_eq!(limits.ratio, None, "the inherited default was enforced");
        assert!(!limits.reached(99.0, 999_999));

        cfg.set("queue.share_limit_enabled", &true);
        assert_eq!(ShareLimits::global(&cfg).ratio, Some(2.0));
    }

    #[test]
    fn no_limits_never_stop() {
        let limits = ShareLimits {
            ratio: None,
            seed_minutes: None,
            action: ShareAction::Pause,
        };
        assert!(!limits.reached(999.0, 999_999));
    }

    /// A per-torrent value replaces the global one; `None` leaves it alone.
    #[test]
    fn overrides_replace_only_what_they_set() {
        let global = ShareLimits {
            ratio: Some(2.0),
            seed_minutes: Some(60),
            action: ShareAction::Pause,
        };
        let mine = global.with_overrides(Some(5.0), None);
        assert_eq!(mine.ratio, Some(5.0));
        assert_eq!(mine.seed_minutes, Some(60));

        // Zero from a torrent means "no limit for me", not "stop immediately".
        let unlimited = global.with_overrides(Some(0.0), Some(0));
        assert_eq!(unlimited.ratio, None);
        assert_eq!(unlimited.seed_minutes, None);
        assert!(!unlimited.reached(999.0, 999_999));
    }

    /// An unknown action must never be read as one that deletes.
    #[test]
    fn an_unrecognised_action_pauses() {
        assert_eq!(ShareAction::parse("pause"), ShareAction::Pause);
        assert_eq!(
            ShareAction::parse("remove"),
            ShareAction::Remove { with_data: false }
        );
        assert_eq!(
            ShareAction::parse("remove_with_data"),
            ShareAction::Remove { with_data: true }
        );
        assert_eq!(ShareAction::parse("nonsense"), ShareAction::Pause);
        assert_eq!(ShareAction::parse(""), ShareAction::Pause);
    }
}
