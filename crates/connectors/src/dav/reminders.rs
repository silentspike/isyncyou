//! Bounded, content-free reminder plans for server-expanded calendar instances.
//!
//! A plan is not a delivery claim. Unsupported or ambiguous time semantics fail
//! closed; callers must re-check access and use idempotent native notification
//! identities before dispatch.

use chrono::{Datelike, LocalResult, NaiveDate, NaiveDateTime, TimeZone};
use ical::tree::cst::{IcalCst, IcalItem};
use ical::tree::line::IcalLine;

#[derive(Clone, PartialEq, Eq)]
pub struct Reminder {
    pub uid: String,
    pub recurrence_id: String,
    pub alarm_id: String,
    pub trigger_at: i64,
}

impl Reminder {
    /// Stable platform tag without account, event or alarm content in cleartext.
    pub fn notification_tag(&self, scope: &isyncyou_store::ProviderScope) -> String {
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        for value in [&scope.account_id, &scope.origin, &scope.service, &scope.collection, &self.uid, &self.recurrence_id, &self.alarm_id] {
            ctx.update(&(value.len() as u64).to_be_bytes());
            ctx.update(value.as_bytes());
        }
        ctx.update(&self.trigger_at.to_be_bytes());
        ctx.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn kind(node: &IcalCst<'_>) -> Option<String> {
    node.begin.as_ref().map(|line| line.raw_value_str().to_ascii_uppercase())
}

fn lines<'a, 'b>(node: &'b IcalCst<'a>, property: &str) -> Vec<&'b IcalLine<'a>> {
    node.items.iter().filter_map(|entry| match entry {
        IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case(property) => Some(line),
        _ => None,
    }).collect()
}

fn one<'a, 'b>(node: &'b IcalCst<'a>, property: &str) -> Result<Option<&'b IcalLine<'a>>, String> {
    let found = lines(node, property);
    if found.len() > 1 { return Err("Ambiguous reminder property".into()); }
    Ok(found.into_iter().next())
}

fn parameter(line: &IcalLine<'_>, key: &str) -> Result<Option<String>, String> {
    let params: Vec<_> = line.params.iter().filter(|p| p.name.get().eq_ignore_ascii_case(key)).collect();
    if params.len() > 1 || params.first().is_some_and(|p| p.values.len() != 1) { return Err("Ambiguous reminder parameter".into()); }
    Ok(params.first().map(|p| p.values[0].get().trim_matches('"').to_owned()))
}

fn seconds(value: &str) -> Result<(i64, i64), String> {
    let (sign, unsigned) = match value.as_bytes().first() { Some(b'-') => (-1i64, &value[1..]), Some(b'+') => (1, &value[1..]), _ => (1, value) };
    let duration = iso8601::duration(unsigned).map_err(|_| "Invalid alarm duration")?;
    let (seconds, days) = match duration {
        iso8601::Duration::Weeks(weeks) => (i64::from(weeks).checked_mul(604800).ok_or("Alarm duration overflow")?, i64::from(weeks) * 7),
        iso8601::Duration::YMDHMS {year, month, day, hour, minute, second, millisecond} => {
            if year != 0 || month != 0 || millisecond != 0 { return Err("Unsupported alarm duration".into()); }
            let seconds = i64::from(day)*86400 + i64::from(hour)*3600 + i64::from(minute)*60 + i64::from(second);
            (seconds, i64::from(day))
        }
    };
    let seconds = seconds.checked_mul(sign).ok_or("Alarm duration overflow")?;
    if seconds.unsigned_abs() > 366 * 86400 { return Err("Alarm duration exceeds planning limit".into()); }
    Ok((seconds, days * sign))
}

fn local_instant(local: NaiveDateTime, tzid: &str, calendar: &ical::ical::Ical<'_>) -> Result<i64, String> {
    if let Some(zone) = ical::tz::IcalTz::of_calendar(calendar, tzid) {
        if zone.observances.is_empty() { return Err("Calendar timezone has no transitions".into()); }
        let civil = ical::recur::IcalRecurDateTime::parse(&local.format("%Y%m%dT%H%M%S").to_string()).map_err(|_| "Invalid local alarm time")?;
        return zone.resolve(civil).instant(civil).ok_or_else(|| "Alarm time falls in a timezone gap".into());
    }
    let zone: chrono_tz::Tz = tzid.parse().map_err(|_| "Unknown alarm timezone")?;
    match zone.from_local_datetime(&local) {
        LocalResult::Single(value) => Ok(value.timestamp()),
        LocalResult::Ambiguous(a, b) => Ok(a.timestamp().min(b.timestamp())),
        LocalResult::None => Err("Alarm time falls in a timezone gap".into()),
    }
}

fn event_time(line: &IcalLine<'_>, fallback_tzid: Option<&str>, calendar: &ical::ical::Ical<'_>) -> Result<(i64, bool), String> {
    let raw = line.value.decode();
    let value = raw.as_ref();
    if value.ends_with('Z') && value.len() == 16 {
        let date = ical::recur::IcalRecurDateTime::parse(value).map_err(|_| "Invalid UTC alarm anchor")?;
        return Ok((date.seconds(), false));
    }
    let date_only = value.len() == 8;
    let local = if date_only {
        NaiveDate::parse_from_str(value, "%Y%m%d").map_err(|_| "Invalid all-day alarm anchor")?.and_hms_opt(0,0,0).ok_or("Invalid all-day alarm anchor")?
    } else {
        NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").map_err(|_| "Invalid local alarm anchor")?
    };
    let tzid = parameter(line,"TZID")?.or_else(|| fallback_tzid.map(str::to_owned)).ok_or("Alarm timezone unavailable")?;
    Ok((local_instant(local,&tzid,calendar)?, date_only))
}

// RFC 5545 applies nominal weeks/days in local civil time before exact
// hours/minutes/seconds. Adding 86400 seconds across a DST change is wrong.
fn offset_time(line: &IcalLine<'_>, fallback_tzid: Option<&str>, calendar: &ical::ical::Ical<'_>, offset: i64, days: i64) -> Result<i64, String> {
    if days == 0 { return event_time(line, fallback_tzid, calendar)?.0.checked_add(offset).ok_or_else(|| "Alarm timestamp overflow".into()); }
    let raw = line.value.decode();
    if raw.ends_with('Z') { return event_time(line, fallback_tzid, calendar)?.0.checked_add(offset).ok_or_else(|| "Alarm timestamp overflow".into()); }
    let local = if raw.len() == 8 {
        NaiveDate::parse_from_str(&raw, "%Y%m%d").map_err(|_| "Invalid all-day alarm anchor")?.and_hms_opt(0, 0, 0).ok_or("Invalid all-day alarm anchor")?
    } else {
        NaiveDateTime::parse_from_str(&raw, "%Y%m%dT%H%M%S").map_err(|_| "Invalid local alarm anchor")?
    };
    let tzid = parameter(line, "TZID")?.or_else(|| fallback_tzid.map(str::to_owned)).ok_or("Alarm timezone unavailable")?;
    let remainder = offset - days * 86400;
    let shifted = local.checked_add_signed(chrono::Duration::days(days)).ok_or("Alarm timestamp overflow")?;
    local_instant(shifted, &tzid, calendar)?.checked_add(remainder).ok_or_else(|| "Alarm timestamp overflow".into())
}

/// Plan up to 512 DISPLAY occurrences within a <=31-day UTC window from a
/// server-expanded response. Unsupported alarms return a concrete error, never
/// a falsely complete plan. Caller supplies the user's zone for floating and
/// all-day values and the original resource for embedded VTIMEZONE rules.
pub fn plan(expanded: &[u8], original: &[u8], fallback_tzid: Option<&str>, window_start: i64, window_end: i64) -> Result<Vec<Reminder>, String> {
    if expanded.len() > 16*1024*1024 || original.len() > 16*1024*1024 || window_end <= window_start || window_end.saturating_sub(window_start) > 31*86400 { return Err("Invalid reminder planning window".into()); }
    let window_year = chrono::DateTime::from_timestamp(window_start,0).ok_or("Invalid reminder planning timestamp")?.year();
    let original_tree = IcalCst::parse(original).map_err(|_| "Invalid original calendar")?;
    if original_tree.to_bytes() != original { return Err("Original calendar is incomplete".into()); }
    // Validate calendar/timezone structure separately from alarms: the parser's
    // property validator currently rejects RFC-valid absolute DATE-TIME
    // TRIGGER values. Alarm fields are validated explicitly below.
    let mut validation_tree = original_tree.clone();
    for entry in &mut validation_tree.items {
        if let IcalItem::Component(component) = entry {
            if matches!(kind(component).as_deref(),Some("VEVENT" | "VTODO")) {
                component.items.retain(|item| !matches!(item,IcalItem::Component(child) if kind(child).as_deref() == Some("VALARM")));
                // The model validator requires every referenced TZID to have
                // an embedded VTIMEZONE. This planner also supports IANA TZIDs
                // via chrono-tz, so validate the remaining calendar structure
                // without those references; event_time independently resolves
                // every anchor against the preserved original TZID and fails
                // closed for unknown zones and nonexistent local times.
                for item in &mut component.items {
                    if let IcalItem::Prop(line) = item {
                        line.params.retain(|param| !param.name.get().eq_ignore_ascii_case("TZID"));
                    }
                }
            }
        }
    }
    let original_model = validation_tree.decode().validate().map_err(|_| "Calendar or timezone cannot be validated")?.into_inner();
    let tree = IcalCst::parse(expanded).map_err(|_| "Invalid expanded calendar")?;
    if tree.to_bytes() != expanded { return Err("Expanded calendar is incomplete".into()); }
    let mut result = Vec::new();
    let mut occurrences = 0usize;
    let mut alarm_count = 0usize;
    for entry in &tree.items {
        let IcalItem::Component(event) = entry else { continue; };
        if !matches!(kind(event).as_deref(),Some("VEVENT" | "VTODO")) { continue; }
        occurrences += 1;
        if occurrences > 4096 { return Err("Expanded occurrence limit exceeded".into()); }
        if one(event,"RRULE")?.is_some() || one(event,"RDATE")?.is_some() { return Err("Reminder source was not fully expanded".into()); }
        if one(event,"STATUS")?.is_some_and(|line| matches!(line.value.decode().to_ascii_uppercase().as_str(), "CANCELLED" | "COMPLETED")) { continue; }
        let uid = one(event,"UID")?.ok_or("Reminder UID missing")?.value.decode().into_owned();
        if uid.is_empty() { return Err("Reminder UID missing".into()); }
        let start = one(event,"DTSTART")?;
        let due = one(event,"DUE")?;
        let recurrence_id = one(event,"RECURRENCE-ID")?.map(|line| line.value.decode().into_owned()).or_else(|| start.or(due).map(|line| line.value.decode().into_owned())).ok_or("Reminder occurrence identity missing")?;
        for child in &event.items {
            let IcalItem::Component(alarm) = child else { continue; };
            if kind(alarm).as_deref() != Some("VALARM") { continue; }
            alarm_count += 1;
            if alarm_count > 4096 { return Err("Alarm count exceeds limit".into()); }
            let action = one(alarm,"ACTION")?.ok_or("Alarm action missing")?.value.decode();
            if !action.eq_ignore_ascii_case("DISPLAY") { return Err("Unsupported alarm action".into()); }
            let trigger = one(alarm,"TRIGGER")?.ok_or("Alarm trigger missing")?;
            let trigger_value = trigger.value.decode();
            let base = if parameter(trigger,"VALUE")?.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("DATE-TIME")) {
                if !trigger_value.ends_with('Z') { return Err("Absolute alarm trigger must be UTC".into()); }
                if parameter(trigger,"RELATED")?.is_some() { return Err("Absolute alarm cannot specify RELATED".into()); }
                if one(event,"RECURRENCE-ID")?.is_some() { return Err("Absolute recurring alarm needs series-level deduplication".into()); }
                event_time(trigger,None,&original_model)?.0
            } else {
                let (offset, days) = seconds(&trigger_value)?;
                let related = parameter(trigger,"RELATED")?.unwrap_or_else(|| "START".into());
                let anchor = if related.eq_ignore_ascii_case("START") { start }
                    else if related.eq_ignore_ascii_case("END") { one(event,"DTEND")?.or(due) }
                    else { return Err("Unsupported alarm relation".into()); };
                let anchor = anchor.ok_or("Alarm anchor missing")?;
                let anchor_value = anchor.value.decode();
                let civil = ical::recur::IcalRecurDateTime::parse(&anchor_value).map_err(|_| "Invalid reminder anchor")?;
                if civil.year < window_year.saturating_sub(2) || civil.year > window_year.saturating_add(2) { return Err("Reminder anchor exceeds supported horizon".into()); }
                offset_time(anchor,fallback_tzid,&original_model,offset,days)?
            };
            let uid_field = one(alarm,"UID")?.map(|line| line.value.decode().into_owned());
            let alarm_id = uid_field.filter(|s| !s.is_empty()).unwrap_or_else(|| ring::digest::digest(&ring::digest::SHA256,&alarm.to_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect());
            let repeat = match one(alarm,"REPEAT")? { Some(line) => { let repeat = line.value.decode().parse::<usize>().map_err(|_| "Invalid alarm repeat")?; if repeat == 0 { return Err("Invalid alarm repeat".into()); } repeat }, None => 0 };
            if repeat > 100 { return Err("Alarm repeat exceeds limit".into()); }
            let interval = match (repeat,one(alarm,"DURATION")?) {
                (0,None) => 0,
                (0,Some(_)) | (_,None) => return Err("Invalid alarm repeat duration".into()),
                (_,Some(line)) => { let (value, days) = seconds(&line.value.decode())?; if value <= 0 || days != 0 { return Err("Unsupported alarm repeat duration".into()); } value },
            };
            for n in 0..=repeat {
                let trigger_at = base.checked_add(interval.checked_mul(n as i64).ok_or("Alarm timestamp overflow")?).ok_or("Alarm timestamp overflow")?;
                if trigger_at >= window_start && trigger_at < window_end {
                    if result.len() >= 512 { return Err("Alarm plan exceeds limit".into()); }
                    result.push(Reminder {uid:uid.clone(),recurrence_id:recurrence_id.clone(),alarm_id:alarm_id.clone(),trigger_at});
                }
            }
        }
    }
    result.sort_by_key(|r| r.trigger_at);
    let mut seen = std::collections::HashSet::new();
    if result.iter().any(|r| !seen.insert((r.uid.clone(),r.recurrence_id.clone(),r.alarm_id.clone(),r.trigger_at))) { return Err("Duplicate reminder identity".into()); }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_utc_alarm_and_repeat_without_content() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261001T100000Z\r\nDTSTART:20261028T100000Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nREPEAT:1\r\nDURATION:PT5M\r\nDESCRIPTION:private\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let plan = plan(raw,raw,None,1793180000,1793190000).unwrap();
        assert_eq!(plan.len(),2);
        assert_eq!(plan[1].trigger_at - plan[0].trigger_at,300);
        assert!(!plan[0].alarm_id.contains("private"));
    }

    #[test]
    fn ambiguous_or_unexpanded_input_fails_closed() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTART:20261028T100000\r\nRRULE:FREQ=DAILY\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(plan(raw,raw,None,1793180000,1793190000).is_err());
    }

    #[test]
    fn fall_fold_uses_first_occurrence_and_spring_gap_is_explicit() {
        let fold = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261001T100000Z\r\nDTSTART;TZID=Europe/Vienna:20261025T023000\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nDESCRIPTION:Alarm\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let alarms = plan(fold,fold,None,1792880000,1792895000).unwrap();
        assert_eq!(alarms.len(),1);
        assert_eq!(alarms[0].trigger_at,1792888200-900);
        let gap = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20260301T100000Z\r\nDTSTART;TZID=Europe/Vienna:20260329T023000\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nDESCRIPTION:Alarm\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(plan(gap,gap,None,1774740000,1774750000).is_err());
    }

    #[test]
    fn nominal_day_alarm_uses_civil_time_across_dst() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261001T100000Z\r\nDTSTART:20261028T100000Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-P1D\r\nDESCRIPTION:Alarm\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let alarms = plan(raw,raw,None,1793090000,1793100000).unwrap();
        assert_eq!(alarms[0].trigger_at, 1793095200);
        let local = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261001T100000Z\r\nDTSTART;TZID=Europe/Vienna:20261026T100000\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-P1D\r\nDESCRIPTION:Alarm\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let alarms = plan(local,local,None,1792910000,1792920000).unwrap();
        assert_eq!(alarms[0].trigger_at,1792918800);
        // Hours remain exact even when they exceed 24; they must not be
        // silently promoted to another nominal day across the DST boundary.
        let mixed = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261001T100000Z\r\nDTSTART;TZID=Europe/Vienna:20261026T100000\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-P1DT25H\r\nDESCRIPTION:Alarm\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let alarms = plan(mixed,mixed,None,1792820000,1792830000).unwrap();
        assert_eq!(alarms[0].trigger_at,1792918800-25*3600);
    }

    #[test]
    fn absolute_utc_trigger_is_independent_of_event_start() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261001T100000Z\r\nDTSTART:20261028T100000Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER;VALUE=DATE-TIME:20261028T094500Z\r\nDESCRIPTION:Alarm\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let alarms = plan(raw,raw,None,1793180000,1793190000).unwrap();
        assert_eq!(alarms.len(),1);
        assert_eq!(alarms[0].trigger_at,1793181600-900);
    }

    #[test]
    fn task_due_alarm_uses_native_due_as_end_and_occurrence_identity() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VTODO\r\nUID:task\r\nDTSTAMP:20261001T100000Z\r\nDUE:20261028T100000Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER;RELATED=END:-PT15M\r\nDESCRIPTION:Alarm\r\nEND:VALARM\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let alarms = plan(raw,raw,None,1793180000,1793190000).unwrap();
        assert_eq!(alarms.len(),1);
        assert_eq!(alarms[0].recurrence_id,"20261028T100000Z");
        assert_eq!(alarms[0].trigger_at,1793181600-900);
    }

    #[test]
    fn expanded_absolute_series_alarm_requires_series_deduplication() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261001T100000Z\r\nDTSTART:20261028T100000Z\r\nRECURRENCE-ID:20261028T100000Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER;VALUE=DATE-TIME:20261028T094500Z\r\nDESCRIPTION:Alarm\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(plan(raw,raw,None,1793180000,1793190000).is_err());
    }
}
