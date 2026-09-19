//! Time helpers: station-local dates for charts and the timestamp format used by the API.

use chrono::{DateTime, NaiveDate, SecondsFormat, Timelike, Utc};
use chrono_tz::Tz;

/// The local calendar date and hour (0–23) of `ts` in the station time zone.
pub fn local_date_hour(ts: DateTime<Utc>, tz: Tz) -> (NaiveDate, u32) {
    let local = ts.with_timezone(&tz);
    (local.date_naive(), local.hour())
}

/// RFC 3339 in UTC with microsecond precision, e.g. `2026-05-15T10:00:00.000000Z`.
pub fn rfc3339_micros(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Micros, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn formats_with_microseconds() {
        let ts = Utc.with_ymd_and_hms(2026, 5, 15, 10, 0, 0).unwrap();
        assert_eq!(rfc3339_micros(ts), "2026-05-15T10:00:00.000000Z");
    }

    #[test]
    fn local_hour_follows_dst_in_new_york() {
        let tz: Tz = "America/New_York".parse().unwrap();
        // 2026-03-08 is the spring-forward day: 07:30 UTC is 02:30 EST → skipped, so 03:30 EDT.
        let before = Utc.with_ymd_and_hms(2026, 3, 8, 6, 30, 0).unwrap(); // 01:30 EST
        let after = Utc.with_ymd_and_hms(2026, 3, 8, 7, 30, 0).unwrap(); // 03:30 EDT
        let day = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
        assert_eq!(local_date_hour(before, tz), (day, 1));
        assert_eq!(local_date_hour(after, tz), (day, 3));
    }

    #[test]
    fn local_date_can_differ_from_utc_date() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let ts = Utc.with_ymd_and_hms(2026, 7, 1, 2, 0, 0).unwrap(); // 22:00 EDT on June 30
        assert_eq!(
            local_date_hour(ts, tz),
            (NaiveDate::from_ymd_opt(2026, 6, 30).unwrap(), 22)
        );
    }
}
