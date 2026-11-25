//! When to back up again while a drive stays plugged in.

use serde::{Deserialize, Serialize};

pub const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Only back up when the drive is plugged in.
    #[default]
    Off,
    /// Every N minutes while plugged in.
    Every { minutes: u32 },
    /// At fixed times of day (minutes after midnight) on the chosen weekdays (0 = Monday).
    Daily { times: Vec<u16>, days: Vec<u8> },
}

/// The local wall clock, split out so schedule maths can be tested.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    /// Unix seconds.
    pub now: u64,
    /// Today's local calendar date, as days since 1970-01-01.
    pub day: i64,
    /// Converts (local calendar day, minute of that day) to Unix seconds using the
    /// real time-zone rules for that date, so daylight-saving changes are honoured.
    pub to_unix: fn(i64, u16) -> u64,
}

impl Clock {
    pub fn now() -> Clock {
        crate::win::local_clock()
    }
}

/// 0 = Monday ... 6 = Sunday for a day number (1970-01-01 was a Thursday).
pub fn weekday_of(day: i64) -> u8 {
    (day + 3).rem_euclid(7) as u8
}

/// Days since 1970-01-01 for a calendar date (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Calendar date (year, month, day) for a day number.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

pub const MIN_INTERVAL: u32 = 5;
/// Don't retry a scheduled backup that couldn't run more often than this.
pub const RETRY_AFTER_S: u64 = 10 * 60;

pub fn fmt_time(t: u16) -> String {
    format!("{:02}:{:02}", t / 60, t % 60)
}

impl Schedule {
    pub fn is_off(&self) -> bool {
        matches!(self, Schedule::Off)
    }

    /// Evenly spread daily times: 1x = 12:00, 2x = 09:00/18:00, 3x = 09:00/13:00/18:00, 4x = 08/12/16/20.
    pub fn times_per_day(n: usize) -> Schedule {
        let times: Vec<u16> = match n {
            1 => vec![12 * 60],
            2 => vec![9 * 60, 18 * 60],
            3 => vec![9 * 60, 13 * 60, 18 * 60],
            _ => vec![8 * 60, 12 * 60, 16 * 60, 20 * 60],
        };
        Schedule::Daily { times, days: (0..7).collect() }
    }

    fn slots_on(&self, day: i64, c: &Clock) -> Vec<u64> {
        match self {
            Schedule::Daily { times, days } if days.contains(&weekday_of(day)) => {
                let mut v: Vec<u64> = times.iter().map(|t| (c.to_unix)(day, *t)).collect();
                v.sort();
                v
            }
            _ => vec![],
        }
    }

    /// Most recent scheduled moment at or before now.
    fn latest_slot(&self, c: &Clock) -> Option<u64> {
        (0..=7i64).find_map(|d| self.slots_on(c.day - d, c).into_iter().rev().find(|s| *s <= c.now))
    }

    /// Next scheduled moment after now.
    fn next_slot(&self, c: &Clock) -> Option<u64> {
        (0..=8i64).find_map(|d| self.slots_on(c.day + d, c).into_iter().find(|s| *s > c.now))
    }

    /// Should a backup run now, given when the last one finished?
    pub fn is_due(&self, last_backup: Option<u64>, c: &Clock) -> bool {
        match self {
            Schedule::Off => false,
            Schedule::Every { minutes } => {
                let every = (*minutes).max(MIN_INTERVAL) as u64 * 60;
                last_backup.is_none_or(|l| c.now.saturating_sub(l) >= every)
            }
            Schedule::Daily { .. } => match self.latest_slot(c) {
                Some(slot) => last_backup.is_none_or(|l| l < slot),
                None => false,
            },
        }
    }

    /// When the next scheduled backup will happen (Unix seconds), if any.
    pub fn next_run(&self, last_backup: Option<u64>, c: &Clock) -> Option<u64> {
        match self {
            Schedule::Off => None,
            Schedule::Every { minutes } => {
                let every = (*minutes).max(MIN_INTERVAL) as u64 * 60;
                Some(last_backup.map_or(c.now, |l| l + every).max(c.now))
            }
            Schedule::Daily { .. } => {
                if self.is_due(last_backup, c) { Some(c.now) } else { self.next_slot(c) }
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Schedule::Off => "Only when plugged in".into(),
            Schedule::Every { minutes } if minutes % 60 == 0 => {
                let h = minutes / 60;
                if h == 1 { "Every hour while plugged in".into() } else { format!("Every {h} hours while plugged in") }
            }
            Schedule::Every { minutes } => format!("Every {minutes} min while plugged in"),
            Schedule::Daily { times, days } => {
                let mut t: Vec<u16> = times.clone();
                t.sort();
                let times = t.iter().map(|x| fmt_time(*x)).collect::<Vec<_>>().join(", ");
                let days = match days.len() {
                    7 => "daily".to_string(),
                    5 if (0..5).all(|d| days.contains(&d)) => "on weekdays".to_string(),
                    _ => {
                        let mut d = days.clone();
                        d.sort();
                        format!("on {}", d.iter().map(|x| DAY_NAMES[*x as usize % 7]).collect::<Vec<_>>().join(", "))
                    }
                };
                format!("At {times} {days} while plugged in")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Monday 2026-11-23, as if local time == UTC.
    const MON: i64 = 20_780;

    fn utc(day: i64, min: u16) -> u64 {
        (day * 86_400 + min as i64 * 60) as u64
    }

    fn clock(day: i64, h: u16, m: u16) -> Clock {
        Clock { now: utc(MON + day, h * 60 + m), day: MON + day, to_unix: utc }
    }

    fn at(day: i64, h: u16, m: u16) -> u64 {
        utc(MON + day, h * 60 + m)
    }

    #[test]
    fn calendar_helpers() {
        assert_eq!(days_from_civil(2026, 11, 23), MON);
        assert_eq!(civil_from_days(MON), (2026, 11, 23));
        assert_eq!(weekday_of(MON), 0);
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(weekday_of(days_from_civil(2026, 11, 29)), 6); // Sunday
    }

    // A zone that moves clocks back one hour starting on Wednesday (like the end of DST).
    fn dst(day: i64, min: u16) -> u64 {
        let base = (day * 86_400 + min as i64 * 60) as u64;
        if day >= MON + 2 { base + 3600 } else { base }
    }

    #[test]
    fn daylight_saving_change_keeps_wall_clock_time() {
        let s = Schedule::Daily { times: vec![9 * 60], days: (0..7).collect() };
        // Tuesday 10:00, backed up at Tuesday 09:00: next is Wednesday 09:00 *local*,
        // which is 25 hours later in absolute time, not 24.
        let c = Clock { now: dst(MON + 1, 600), day: MON + 1, to_unix: dst };
        let next = s.next_run(Some(dst(MON + 1, 540)), &c).unwrap();
        assert_eq!(next, dst(MON + 2, 540));
        assert_eq!(next - dst(MON + 1, 540), 25 * 3600);
    }

    #[test]
    fn three_times_a_day() {
        let s = Schedule::times_per_day(3); // 09:00, 13:00, 18:00
        // Backed up at 08:00; at 12:00 the 09:00 slot has passed -> due.
        assert!(s.is_due(Some(at(0, 8, 0)), &clock(0, 12, 0)));
        // Backed up at 09:05; at 12:00 nothing new is due.
        assert!(!s.is_due(Some(at(0, 9, 5)), &clock(0, 12, 0)));
        assert_eq!(s.next_run(Some(at(0, 9, 5)), &clock(0, 12, 0)), Some(at(0, 13, 0)));
        // After 18:00 the next one is tomorrow 09:00.
        assert_eq!(s.next_run(Some(at(0, 18, 1)), &clock(0, 20, 0)), Some(at(1, 9, 0)));
        // Never backed up -> due as soon as a slot has passed.
        assert!(s.is_due(None, &clock(0, 9, 0)));
        // Before today's first slot, yesterday's 18:00 still counts if never backed up.
        assert!(s.is_due(None, &clock(0, 8, 59)));
    }

    #[test]
    fn weekdays_only() {
        let s = Schedule::Daily { times: vec![9 * 60], days: vec![0, 1, 2, 3, 4] };
        // Friday 09:30 after a Friday 09:01 backup: next is Monday 09:00.
        assert_eq!(s.next_run(Some(at(4, 9, 1)), &clock(4, 9, 30)), Some(at(7, 9, 0)));
        // Sunday: the latest slot is Friday 09:00, already covered.
        assert!(!s.is_due(Some(at(4, 9, 1)), &clock(6, 12, 0)));
        // Missed Friday's slot (last backup Thursday) -> due on Sunday.
        assert!(s.is_due(Some(at(3, 10, 0)), &clock(6, 12, 0)));
    }

    #[test]
    fn interval() {
        let s = Schedule::Every { minutes: 120 };
        assert!(!s.is_due(Some(at(0, 10, 0)), &clock(0, 11, 59)));
        assert!(s.is_due(Some(at(0, 10, 0)), &clock(0, 12, 0)));
        assert_eq!(s.next_run(Some(at(0, 10, 0)), &clock(0, 11, 0)), Some(at(0, 12, 0)));
        assert_eq!(Schedule::Off.next_run(None, &clock(0, 1, 0)), None);
    }

    #[test]
    fn descriptions() {
        assert_eq!(Schedule::times_per_day(3).describe(), "At 09:00, 13:00, 18:00 daily while plugged in");
        assert_eq!(Schedule::Every { minutes: 180 }.describe(), "Every 3 hours while plugged in");
        assert_eq!(
            Schedule::Daily { times: vec![600], days: vec![0, 1, 2, 3, 4] }.describe(),
            "At 10:00 on weekdays while plugged in"
        );
    }
}
