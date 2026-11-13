//! Gregorian civil dates (UTC), from brokkr's `civil_date`.

/// One day in the proleptic Gregorian calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CivilDate {
    year: i32,
    month: u32,
    day: u32,
}

impl CivilDate {
    pub(crate) fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        let days_in_month = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap_year(year) => 29,
            2 => 28,
            _ => return None,
        };
        if day == 0 || day > days_in_month {
            return None;
        }
        Some(Self { year, month, day })
    }

    pub(crate) fn parse_yyyymmdd(value: &str) -> Option<Self> {
        if value.len() != 8 || !value.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Self::new(
            value[..4].parse().ok()?,
            value[4..6].parse().ok()?,
            value[6..].parse().ok()?,
        )
    }

    /// Today in UTC, read from the system clock.
    pub(crate) fn today() -> Self {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self::from_unix_timestamp(i64::try_from(secs).unwrap_or(i64::MAX)).unwrap_or(Self {
            year: 1970,
            month: 1,
            day: 1,
        })
    }

    pub(crate) fn yyyymmdd(self) -> String {
        format!("{:04}{:02}{:02}", self.year, self.month, self.day)
    }

    pub(crate) fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    pub(crate) fn to_unix_seconds(self, hour: u32, minute: u32, second: u32) -> Option<i64> {
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }

        let year = i64::from(self.year);
        let year = year.checked_sub(if self.month <= 2 { 1 } else { 0 })?;
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let month = i64::from(self.month);
        let shifted_month = if month > 2 { month - 3 } else { month + 9 };
        let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(self.day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era
            .checked_mul(146_097)?
            .checked_add(day_of_era)?
            .checked_sub(719_468)?;

        days.checked_mul(86_400)?
            .checked_add(i64::from(hour) * 3_600)?
            .checked_add(i64::from(minute) * 60)?
            .checked_add(i64::from(second))
    }

    /// The UTC date containing a Unix timestamp.
    pub(crate) fn from_unix_timestamp(seconds: i64) -> Option<Self> {
        let days = seconds.div_euclid(86_400);
        let shifted = days.checked_add(719_468)?;
        let era = if shifted >= 0 {
            shifted
        } else {
            shifted.checked_sub(146_096)?
        } / 146_097;
        let day_of_era = shifted.checked_sub(era.checked_mul(146_097)?)?;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let mut year = year_of_era.checked_add(era.checked_mul(400)?)?;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_prime = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
        let month = month_prime + if month_prime < 10 { 3 } else { -9 };
        year = year.checked_add(i64::from(month <= 2))?;
        Self::new(
            i32::try_from(year).ok()?,
            u32::try_from(month).ok()?,
            u32::try_from(day).ok()?,
        )
    }
}

const fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::CivilDate;

    #[test]
    fn compact_dates_require_a_real_calendar_day() {
        assert_eq!(
            CivilDate::parse_yyyymmdd("20000229").unwrap().iso(),
            "2000-02-29"
        );
        for invalid in [
            "19000229",
            "20260229",
            "20260431",
            "20260001",
            "20260100",
            "2026-411",
            "2026041",
            "202604111",
        ] {
            assert!(CivilDate::parse_yyyymmdd(invalid).is_none());
        }
    }

    #[test]
    fn unix_timestamps_format_as_utc_dates() {
        let d = CivilDate::from_unix_timestamp(1_775_865_600).unwrap();
        assert_eq!(d.yyyymmdd(), "20260411");
        assert_eq!(d.iso(), "2026-04-11");
        assert_eq!(
            CivilDate::from_unix_timestamp(0).unwrap().iso(),
            "1970-01-01"
        );
        assert_eq!(
            CivilDate::from_unix_timestamp(951_782_400).unwrap().iso(),
            "2000-02-29"
        );
    }
}
