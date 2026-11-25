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
    /// Seconds since local midnight.
    pub secs_today: u64,
    /// 0 = Monday ... 6 = Sunday.
    pub weekday: u8,
}

impl Clock {
    pub fn now() -> Clock {
        crate::win::local_clock()
    }
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

    fn slots_on(&self, day_start: u64, weekday: u8) -> Vec<u64> {
        match self {
            Schedule::Daily { times, days } if days.contains(&weekday) => {
                let mut v: Vec<u64> = times.iter().map(|t| day_start + *t as u64 * 60).collect();
                v.sort();
                v
            }
            _ => vec![],
        }
    }

    /// Most recent scheduled moment at or before now.
    fn latest_slot(&self, c: &Clock) -> Option<u64> {
        let midnight = c.now - c.secs_today;
        (0..=7u64).find_map(|d| {
            let day_start = midnight.checked_sub(d * 86_400)?;
            let wd = ((c.weekday as u64 + 7 - d % 7) % 7) as u8;
            self.slots_on(day_start, wd).into_iter().rev().find(|s| *s <= c.now)
        })
    }

    /// Next scheduled moment after now.
    fn next_slot(&self, c: &Clock) -> Option<u64> {
        let midnight = c.now - c.secs_today;
        (0..=7u64).find_map(|d| {
            let wd = ((c.weekday as u64 + d) % 7) as u8;
            self.slots_on(midnight + d * 86_400, wd).into_iter().find(|s| *s > c.now)
        })
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

    // Monday 2026-11-23 00:00 local, as if local == UTC for the test.
    const MON_MIDNIGHT: u64 = 1_795_392_000;

    fn clock(day: u64, h: u64, m: u64) -> Clock {
        let secs_today = h * 3600 + m * 60;
        Clock { now: MON_MIDNIGHT + day * 86_400 + secs_today, secs_today, weekday: (day % 7) as u8 }
    }

    fn at(day: u64, h: u64, m: u64) -> u64 {
        MON_MIDNIGHT + day * 86_400 + h * 3600 + m * 60
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
