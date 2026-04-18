// parse cadence strings into (next_run_at, is_recurring).
//
// grammar:
//   once YYYY-MM-DD HH:MM      -- one-shot absolute
//   in Nh / in Nm / in Nd      -- one-shot relative
//   daily HH:MM                -- recurring daily at local time
//   weekly <dow> HH:MM         -- recurring weekly (dow = mon..sun)
//   every Nh / every Nm        -- recurring interval
//   cron <5-field>             -- escape hatch
//
// times are local. recurring tasks bump next_run_at after each fire.

use anyhow::{anyhow, Context, Result};
use chrono::{
    DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Weekday,
};
use croner::Cron;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    OneShot,
    Recurring,
}

pub fn parse(spec: &str, now: DateTime<Local>) -> Result<(i64, Kind)> {
    let s = spec.trim().to_ascii_lowercase();

    // "once YYYY-MM-DD HH:MM"
    if let Some(rest) = s.strip_prefix("once ") {
        let dt = NaiveDateTime::parse_from_str(rest.trim(), "%Y-%m-%d %H:%M")
            .context("once <YYYY-MM-DD HH:MM>")?;
        let local = Local
            .from_local_datetime(&dt)
            .single()
            .ok_or_else(|| anyhow!("ambiguous local time"))?;
        return Ok((local.timestamp(), Kind::OneShot));
    }

    // "in 2h" / "in 30m" / "in 1d"
    if let Some(rest) = s.strip_prefix("in ") {
        let dur = parse_relative(rest.trim())?;
        return Ok(((now + dur).timestamp(), Kind::OneShot));
    }

    // "daily HH:MM"
    if let Some(rest) = s.strip_prefix("daily ") {
        let t = NaiveTime::parse_from_str(rest.trim(), "%H:%M").context("daily <HH:MM>")?;
        return Ok((next_time_today_or_tomorrow(now, t), Kind::Recurring));
    }

    // "weekly mon 09:00"
    if let Some(rest) = s.strip_prefix("weekly ") {
        let mut parts = rest.splitn(2, ' ');
        let dow_str = parts.next().unwrap_or("");
        let time_str = parts.next().unwrap_or("").trim();
        let dow = parse_dow(dow_str)?;
        let t = NaiveTime::parse_from_str(time_str, "%H:%M").context("weekly <dow> <HH:MM>")?;
        return Ok((next_weekly(now, dow, t), Kind::Recurring));
    }

    // "every 15m" / "every 2h"
    if let Some(rest) = s.strip_prefix("every ") {
        let dur = parse_relative(rest.trim())?;
        if dur < Duration::minutes(1) {
            return Err(anyhow!("minimum interval is 1 minute"));
        }
        return Ok(((now + dur).timestamp(), Kind::Recurring));
    }

    // "cron <5-field>"
    if let Some(rest) = s.strip_prefix("cron ") {
        let cron = Cron::from_str(rest.trim()).context("invalid cron")?;
        let next = cron
            .find_next_occurrence(&now, false)
            .context("no future cron occurrence")?;
        return Ok((next.timestamp(), Kind::Recurring));
    }

    Err(anyhow!("unrecognized cadence: {:?}", spec))
}

/// Compute the next fire for a recurring task that just fired at `last`.
pub fn next_after(spec: &str, last: DateTime<Local>) -> Result<i64> {
    // recurring specs: daily, weekly, every N, cron
    let s = spec.trim().to_ascii_lowercase();

    if let Some(rest) = s.strip_prefix("daily ") {
        let t = NaiveTime::parse_from_str(rest.trim(), "%H:%M")?;
        // tomorrow at t
        let tomorrow = last
            .date_naive()
            .succ_opt()
            .ok_or_else(|| anyhow!("date overflow"))?;
        let dt = NaiveDateTime::new(tomorrow, t);
        return Ok(Local
            .from_local_datetime(&dt)
            .single()
            .ok_or_else(|| anyhow!("ambiguous"))?
            .timestamp());
    }

    if let Some(rest) = s.strip_prefix("weekly ") {
        let mut parts = rest.splitn(2, ' ');
        let dow = parse_dow(parts.next().unwrap_or(""))?;
        let t = NaiveTime::parse_from_str(parts.next().unwrap_or("").trim(), "%H:%M")?;
        return Ok(next_weekly(last + Duration::minutes(1), dow, t));
    }

    if let Some(rest) = s.strip_prefix("every ") {
        let dur = parse_relative(rest.trim())?;
        return Ok((last + dur).timestamp());
    }

    if let Some(rest) = s.strip_prefix("cron ") {
        let cron = Cron::from_str(rest.trim())?;
        return Ok(cron.find_next_occurrence(&last, false)?.timestamp());
    }

    // once / in are one-shot; archive instead of reschedule
    Err(anyhow!("not a recurring cadence: {:?}", spec))
}

fn parse_relative(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.len() < 2 {
        return Err(anyhow!("bad relative duration: {:?}", s));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: i64 = num.trim().parse().context("number")?;
    match unit {
        "s" => Ok(Duration::seconds(n)),
        "m" => Ok(Duration::minutes(n)),
        "h" => Ok(Duration::hours(n)),
        "d" => Ok(Duration::days(n)),
        _ => Err(anyhow!("unit must be s/m/h/d, got {:?}", unit)),
    }
}

fn parse_dow(s: &str) -> Result<Weekday> {
    match s.trim() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        other => Err(anyhow!("unknown day of week: {:?}", other)),
    }
}

fn next_time_today_or_tomorrow(now: DateTime<Local>, t: NaiveTime) -> i64 {
    let today = NaiveDateTime::new(now.date_naive(), t);
    let candidate = Local.from_local_datetime(&today).single();
    if let Some(c) = candidate {
        if c > now {
            return c.timestamp();
        }
    }
    let tomorrow = now.date_naive().succ_opt().unwrap_or(now.date_naive());
    let dt = NaiveDateTime::new(tomorrow, t);
    Local
        .from_local_datetime(&dt)
        .single()
        .map(|c| c.timestamp())
        .unwrap_or_else(|| now.timestamp() + 86_400)
}

fn next_weekly(now: DateTime<Local>, dow: Weekday, t: NaiveTime) -> i64 {
    // walk forward up to 7 days to find next matching weekday at time t > now
    for i in 0..=7 {
        let date: NaiveDate = now.date_naive() + Duration::days(i);
        if date.weekday() == dow {
            let dt = NaiveDateTime::new(date, t);
            if let Some(local) = Local.from_local_datetime(&dt).single() {
                if local > now {
                    return local.timestamp();
                }
            }
        }
    }
    // shouldn't happen, but fall back to +7d
    (now + Duration::days(7)).timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
    }

    #[test]
    fn in_2h() {
        let now = at(2026, 4, 18, 12, 0);
        let (ts, kind) = parse("in 2h", now).unwrap();
        assert_eq!(kind, Kind::OneShot);
        assert_eq!(ts, at(2026, 4, 18, 14, 0).timestamp());
    }

    #[test]
    fn daily_future() {
        let now = at(2026, 4, 18, 7, 30);
        let (ts, kind) = parse("daily 09:00", now).unwrap();
        assert_eq!(kind, Kind::Recurring);
        assert_eq!(ts, at(2026, 4, 18, 9, 0).timestamp());
    }

    #[test]
    fn daily_rolls_to_tomorrow() {
        let now = at(2026, 4, 18, 10, 0);
        let (ts, _) = parse("daily 09:00", now).unwrap();
        assert_eq!(ts, at(2026, 4, 19, 9, 0).timestamp());
    }

    #[test]
    fn every_15m() {
        let now = at(2026, 4, 18, 10, 0);
        let (ts, kind) = parse("every 15m", now).unwrap();
        assert_eq!(kind, Kind::Recurring);
        assert_eq!(ts, at(2026, 4, 18, 10, 15).timestamp());
    }

    #[test]
    fn weekly_mon() {
        // sat -> next monday
        let now = at(2026, 4, 18, 10, 0); // if this is saturday we expect monday 20
        let (ts, _) = parse("weekly mon 09:00", now).unwrap();
        let dt = Local.timestamp_opt(ts, 0).unwrap();
        assert_eq!(dt.weekday(), Weekday::Mon);
    }

    #[test]
    fn reject_sub_minute_interval() {
        let now = at(2026, 4, 18, 10, 0);
        assert!(parse("every 30s", now).is_err());
    }
}
