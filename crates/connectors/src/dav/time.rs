//! Calendar civil-time conversion with a bundled IANA timezone database.

use jiff::{
    civil::DateTime,
    tz::{AmbiguousOffset, TimeZone},
    Timestamp,
};

pub(super) fn parse(value: &str) -> Result<DateTime, String> {
    let civil =
        ical::recur::IcalRecurDateTime::parse(value).map_err(|_| "Invalid calendar time")?;
    DateTime::new(
        civil.year.try_into().map_err(|_| "Invalid calendar time")?,
        civil.month as i8,
        civil.day as i8,
        civil.hour as i8,
        civil.minute as i8,
        civil.second as i8,
        0,
    )
    .map_err(|_| "Invalid calendar time".into())
}

pub(super) fn format(value: DateTime) -> String {
    value.strftime("%Y%m%dT%H%M%S").to_string()
}

pub(super) fn zone(tzid: &str) -> Result<TimeZone, String> {
    TimeZone::get(tzid).map_err(|_| "Unknown calendar timezone".into())
}

pub(super) fn instant(zone: &TimeZone, local: DateTime, allow_fold: bool) -> Result<i64, String> {
    let ambiguous = zone.to_ambiguous_timestamp(local);
    match ambiguous.offset() {
        AmbiguousOffset::Gap { .. } => Err("Local calendar time falls in a timezone gap".into()),
        AmbiguousOffset::Fold { .. } if !allow_fold => {
            Err("Local calendar time is ambiguous".into())
        }
        _ => ambiguous
            .earlier()
            .map(|t| t.as_second())
            .map_err(|_| "Invalid calendar time".into()),
    }
}

pub(super) fn local(zone: &TimeZone, timestamp: i64) -> Result<DateTime, String> {
    Ok(zone
        .to_datetime(Timestamp::from_second(timestamp).map_err(|_| "Invalid calendar timestamp")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vienna_gap_is_rejected_and_fold_uses_earlier_instant_only_when_allowed() {
        let zone = zone("Europe/Vienna").unwrap();
        assert!(instant(&zone, parse("20260329T023000").unwrap(), true).is_err());
        let fold = parse("20261025T023000").unwrap();
        assert!(instant(&zone, fold, false).is_err());
        assert_eq!(instant(&zone, fold, true).unwrap(), 1792888200);
        assert_eq!(format(local(&zone, 1792888200).unwrap()), "20261025T023000");
        assert!(super::zone("not/a-zone").is_err());
    }
}
