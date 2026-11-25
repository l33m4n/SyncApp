//! The background service's scheduling decisions, kept free of Windows UI code so
//! they can be unit tested: which plugged-in drives are due for a scheduled backup,
//! and when the service should next wake up.

use std::collections::HashMap;

use crate::config::{Config, DriveReg};
use crate::schedule::{Clock, RETRY_AFTER_S};

/// While Battery saver blocks scheduled backups, look again this often.
pub const BATTERY_SAVER_RECHECK_S: u64 = 15 * 60;

pub struct Planner<'a> {
    pub cfg: &'a Config,
    /// Plugged-in backup drives: root ("E:\") -> drive id.
    pub backup_drives: &'a HashMap<String, String>,
    /// Drive ids with a backup currently running.
    pub busy: &'a [String],
    /// Unix time of each drive's last completed backup.
    pub last_backup: &'a HashMap<String, u64>,
    /// Seconds since each drive's last failed attempt (only drives whose last attempt failed).
    pub failed_ago: &'a HashMap<String, u64>,
    /// Windows Battery saver is on right now.
    pub battery_saver: bool,
}

impl Planner<'_> {
    /// Plugged-in drives that have a schedule and aren't already being backed up.
    fn candidates(&self) -> Vec<(&String, &DriveReg)> {
        let mut v: Vec<(&String, &DriveReg)> = self
            .backup_drives
            .iter()
            .filter_map(|(root, id)| self.cfg.drive(id).map(|reg| (root, reg)))
            .filter(|(_, reg)| reg.auto_backup && !reg.schedule.is_off())
            .filter(|(_, reg)| !self.busy.iter().any(|b| *b == reg.drive_id))
            .collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v
    }

    fn held_back_by_battery(&self) -> bool {
        self.battery_saver && self.cfg.skip_on_battery_saver
    }

    fn retry_ok(&self, id: &str) -> bool {
        self.failed_ago.get(id).is_none_or(|ago| *ago >= RETRY_AFTER_S)
    }

    /// Drives due for a scheduled backup right now: (root, drive id).
    pub fn due(&self, clock: &Clock) -> Vec<(String, String)> {
        if self.cfg.paused || self.held_back_by_battery() {
            return vec![];
        }
        self.candidates()
            .into_iter()
            .filter(|(_, reg)| reg.schedule.is_due(self.last_backup.get(&reg.drive_id).copied(), clock))
            .filter(|(_, reg)| self.retry_ok(&reg.drive_id))
            .map(|(root, reg)| (root.clone(), reg.drive_id.clone()))
            .collect()
    }

    /// Whether Battery saver is what's stopping a backup that would otherwise be due.
    pub fn postponed_by_battery(&self, clock: &Clock) -> bool {
        self.held_back_by_battery()
            && !self.cfg.paused
            && self
                .candidates()
                .iter()
                .any(|(_, reg)| reg.schedule.is_due(self.last_backup.get(&reg.drive_id).copied(), clock))
    }

    /// When to wake up next (Unix seconds), or None if nothing is scheduled.
    pub fn next_wake(&self, clock: &Clock) -> Option<u64> {
        if self.cfg.paused {
            return None;
        }
        let candidates = self.candidates();
        if candidates.is_empty() {
            return None;
        }
        if self.held_back_by_battery() {
            return Some(clock.now + BATTERY_SAVER_RECHECK_S);
        }
        candidates
            .iter()
            .filter_map(|(_, reg)| {
                let t = reg.schedule.next_run(self.last_backup.get(&reg.drive_id).copied(), clock)?;
                let retry_at = self
                    .failed_ago
                    .get(&reg.drive_id)
                    .map(|ago| clock.now + RETRY_AFTER_S.saturating_sub(*ago));
                Some(retry_at.map_or(t, |r| t.max(r)))
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DriveReg;
    use crate::schedule::Schedule;

    const DAY: i64 = 20_780; // a Monday

    fn utc(day: i64, min: u16) -> u64 {
        (day * 86_400 + min as i64 * 60) as u64
    }

    fn clock(h: u16, m: u16) -> Clock {
        Clock { now: utc(DAY, h * 60 + m), day: DAY, to_unix: utc }
    }

    fn drive(id: &str, schedule: Schedule) -> DriveReg {
        DriveReg {
            drive_id: id.into(),
            name: id.into(),
            volume_serial: None,
            auto_backup: true,
            sources: None,
            schedule,
            repeat_minutes: 0,
            registered_at: 0,
        }
    }

    struct World {
        cfg: Config,
        plugged: HashMap<String, String>,
        busy: Vec<String>,
        last: HashMap<String, u64>,
        failed: HashMap<String, u64>,
        battery: bool,
    }

    impl World {
        fn new() -> World {
            let mut cfg = Config { skip_on_battery_saver: true, ..Default::default() };
            cfg.drives.push(drive("hourly", Schedule::Every { minutes: 60 }));
            cfg.drives.push(drive("thrice", Schedule::times_per_day(3)));
            cfg.drives.push(drive("off", Schedule::Off));
            let plugged = [("E:\\", "hourly"), ("F:\\", "thrice"), ("G:\\", "off")]
                .iter()
                .map(|(r, i)| (r.to_string(), i.to_string()))
                .collect();
            World { cfg, plugged, busy: vec![], last: HashMap::new(), failed: HashMap::new(), battery: false }
        }
        fn planner(&self) -> Planner<'_> {
            Planner {
                cfg: &self.cfg,
                backup_drives: &self.plugged,
                busy: &self.busy,
                last_backup: &self.last,
                failed_ago: &self.failed,
                battery_saver: self.battery,
            }
        }
    }

    #[test]
    fn due_drives_and_next_wake() {
        let mut w = World::new();
        w.last.insert("hourly".into(), utc(DAY, 9 * 60 + 30)); // 09:30
        w.last.insert("thrice".into(), utc(DAY, 9 * 60 + 5)); // after the 09:00 slot
        // 10:00: hourly not yet due (10:30), thrice next at 13:00.
        assert!(w.planner().due(&clock(10, 0)).is_empty());
        assert_eq!(w.planner().next_wake(&clock(10, 0)), Some(utc(DAY, 10 * 60 + 30)));
        // 10:31: only the hourly drive is due; the "off" drive never is.
        assert_eq!(w.planner().due(&clock(10, 31)), vec![("E:\\".to_string(), "hourly".to_string())]);
    }

    #[test]
    fn paused_auto_off_busy_and_unplugged_are_skipped() {
        let mut w = World::new();
        w.cfg.paused = true;
        assert!(w.planner().due(&clock(23, 0)).is_empty());
        assert_eq!(w.planner().next_wake(&clock(23, 0)), None);
        w.cfg.paused = false;
        w.busy.push("hourly".into());
        w.cfg.drive_mut("thrice").unwrap().auto_backup = false;
        assert!(w.planner().due(&clock(23, 0)).is_empty());
        w.busy.clear();
        w.plugged.remove("E:\\");
        assert!(w.planner().due(&clock(23, 0)).is_empty());
        assert_eq!(w.planner().next_wake(&clock(23, 0)), None);
    }

    #[test]
    fn failed_attempts_wait_before_retrying() {
        let mut w = World::new();
        w.plugged.retain(|_, id| id == "hourly");
        w.failed.insert("hourly".into(), 120); // failed 2 minutes ago
        assert!(w.planner().due(&clock(12, 0)).is_empty());
        assert_eq!(w.planner().next_wake(&clock(12, 0)), Some(clock(12, 0).now + RETRY_AFTER_S - 120));
        w.failed.insert("hourly".into(), RETRY_AFTER_S);
        assert_eq!(w.planner().due(&clock(12, 0)).len(), 1);
    }

    #[test]
    fn battery_saver_postpones_scheduled_backups() {
        let mut w = World::new();
        w.battery = true;
        assert!(w.planner().due(&clock(12, 0)).is_empty());
        assert!(w.planner().postponed_by_battery(&clock(12, 0)));
        assert_eq!(w.planner().next_wake(&clock(12, 0)), Some(clock(12, 0).now + BATTERY_SAVER_RECHECK_S));
        // Turned off in Settings: battery saver doesn't matter.
        w.cfg.skip_on_battery_saver = false;
        assert_eq!(w.planner().due(&clock(12, 0)).len(), 2);
    }
}
