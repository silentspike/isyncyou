//! Lossless iCalendar edits and side-effect-suppressed recovery projections.
//!
//! The native concrete syntax tree retains untouched unknown fields, parameters,
//! time zones, folds and nested components. Callers must use conditional DAV
//! writes; this module never sends or overwrites a resource.

use ical::tree::cst::{IcalCst, IcalItem};
use ical::tree::line::IcalLine;
use std::collections::{HashMap, HashSet};

fn name(node: &IcalCst<'_>) -> Option<String> {
    node.begin.as_ref().map(|line| line.raw_value_str().to_ascii_uppercase())
}

fn values(node: &IcalCst<'_>, key: &str) -> Vec<String> {
    node.items.iter().filter_map(|item| match item {
        IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case(key) => Some(line.value.decode().into_owned()),
        _ => None,
    }).collect()
}

fn parse(raw: &[u8]) -> Result<IcalCst<'_>, String> {
    if raw.len() > 16 * 1024 * 1024 { return Err("Calendar resource exceeds size limit".into()); }
    let tree = IcalCst::parse(raw).map_err(|_| "Invalid calendar resource")?;
    if name(&tree).as_deref() != Some("VCALENDAR") || tree.to_bytes() != raw { return Err("Calendar must be a single complete resource".into()); }
    Ok(tree)
}

fn folded_content_line(value: &str) -> String {
    let mut out = String::new();
    let mut octets = 0;
    for ch in value.chars() {
        let size = ch.len_utf8();
        if octets + size > 75 { out.push_str("\r\n "); octets = 1; }
        out.push(ch);
        octets += size;
    }
    out.push_str("\r\n");
    out
}

fn native_text(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) { return Err("Invalid display text".into()); }
    Ok(value.replace('\\',"\\\\").replace(';',"\\;").replace(',',"\\,"))
}

/// Create a deliberately simple event, task, or vCard from typed fields.
/// UID and timestamp are generated once by the caller and included in the
/// effect-bound approval. No scheduling recipients or relationships exist.
pub fn create_simple(kind: &str, uid: &str, timestamp: &str, name: &str, start: Option<&str>, end: Option<&str>) -> Result<Vec<u8>, String> {
    let name = native_text(name)?;
    if uid.is_empty() || uid.len() > 128 || !uid.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b,b'-' | b'_')) { return Err("Invalid new UID".into()); }
    let utc = |s: &str| s.len() == 16 && s.as_bytes()[8] == b'T' && s.ends_with('Z') && ical::recur::IcalRecurDateTime::parse(s).is_ok();
    if !utc(timestamp) { return Err("Invalid creation timestamp".into()); }
    let bytes = match kind {
        "contact" if start.is_none() && end.is_none() => format!("BEGIN:VCARD\r\nVERSION:4.0\r\n{}{}END:VCARD\r\n",folded_content_line(&format!("UID:{uid}")),folded_content_line(&format!("FN:{name}"))),
        "task" if start.is_none() && end.is_none() => format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//iSyncYou//Groupware//EN\r\nBEGIN:VTODO\r\nUID:{uid}\r\nDTSTAMP:{timestamp}\r\n{}END:VTODO\r\nEND:VCALENDAR\r\n",folded_content_line(&format!("SUMMARY:{name}"))),
        "event" if start.is_some_and(utc) && end.is_some_and(utc) && start < end => format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//iSyncYou//Groupware//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:{timestamp}\r\nDTSTART:{}\r\nDTEND:{}\r\n{}END:VEVENT\r\nEND:VCALENDAR\r\n",start.unwrap_or_default(),end.unwrap_or_default(),folded_content_line(&format!("SUMMARY:{name}"))),
        _ => return Err("Invalid new resource fields".into()),
    };
    Ok(bytes.into_bytes())
}

/// Create one scheduled event with explicit, bounded mailto recipients.
pub fn create_invitation(uid: &str, timestamp: &str, name: &str, start: &str, end: &str, organizer: &str, attendees: &[String]) -> Result<Vec<u8>, String> {
    let valid_address = |address: &str| {
        let Some(value) = address.strip_prefix("mailto:") else { return false; };
        let Some((local,domain)) = value.split_once('@') else { return false; };
        !local.is_empty() && !local.starts_with('.') && !local.ends_with('.') && !local.contains("..") && !domain.is_empty() && domain.split('.').all(|part| !part.is_empty() && !part.starts_with('-') && !part.ends_with('-') && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) && value.len() <= 320 && value.bytes().filter(|b| *b == b'@').count() == 1 && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b,b'@' | b'.' | b'_' | b'-' | b'+'))
    };
    if !valid_address(organizer) || attendees.is_empty() || attendees.len() > 100 || attendees.iter().any(|a| !valid_address(a) || a == organizer) || attendees.iter().collect::<HashSet<_>>().len() != attendees.len() { return Err("Invalid invitation identities".into()); }
    let original = create_simple("event",uid,timestamp,name,Some(start),Some(end))?;
    let mut tree = parse(&original)?;
    let mut count = 0;
    for item in &mut tree.items {
        let IcalItem::Component(event) = item else { continue; };
        if !event.begin.as_ref().is_some_and(|line| line.raw_value_str().eq_ignore_ascii_case("VEVENT")) { continue; }
        count += 1;
        event.items.push(IcalItem::Prop(IcalLine::text("ORGANIZER",organizer.to_owned())));
        for address in attendees {
            let mut line = IcalLine::text("ATTENDEE",address.clone());
            line.params.push(ical::tree::param::node::IcalParamNode::parse("RSVP=TRUE"));
            event.items.push(IcalItem::Prop(line));
        }
    }
    if count != 1 { return Err("New event is ambiguous".into()); }
    Ok(tree.to_bytes())
}

/// Conservative metadata for an atomically maintained task-parent index.
/// Invalid or ambiguous resources must remain stored but cannot authorize
/// parent-graph edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskIndex {
    pub uid: String,
    pub parent_uid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceIndex {
    Event { uid: String },
    Task(TaskIndex),
}

/// Whether a validated resource contains event/task alarms. Detailed trigger
/// semantics are checked separately when a live plan is generated.
pub fn resource_has_alarms(raw: &[u8]) -> Result<bool, String> {
    let tree = parse(raw)?;
    let mut found = false;
    for entry in &tree.items {
        let IcalItem::Component(component) = entry else { continue; };
        if !matches!(name(component).as_deref(),Some("VEVENT" | "VTODO")) { continue; }
        for nested in &component.items {
            if let IcalItem::Component(alarm) = nested {
                if name(alarm).as_deref() == Some("VALARM") { found = true; }
            }
        }
    }
    Ok(found)
}

/// A bounded deletion preview for an unscheduled, non-recurring event.
/// More complex resources need explicit scheduling and series previews.
pub fn preview_event_delete(raw: &[u8]) -> Result<(String, String), String> {
    let tree = parse(raw)?;
    fn has_attachment(node: &IcalCst<'_>) -> bool {
        node.items.iter().any(|item| match item {
            IcalItem::Prop(line) => line.name.get().eq_ignore_ascii_case("ATTACH"),
            IcalItem::Component(child) => has_attachment(child),
            IcalItem::Opaque(_) => true,
        })
    }
    if has_attachment(&tree) { return Err("Attachment or unparsed data requires an expanded delete preview".into()); }
    if !values(&tree,"METHOD").is_empty() { return Err("Scheduled resource requires a scheduling preview".into()); }
    if tree.items.iter().any(|item| matches!(item,IcalItem::Opaque(_))) { return Err("Calendar contains unparsed data".into()); }
    let mut event = None;
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() == Some("VTIMEZONE") { continue; }
        if name(child).as_deref() != Some("VEVENT") || event.replace(child).is_some() { return Err("Delete requires a single event".into()); }
    }
    let event = event.ok_or("Event missing")?;
    if ["ORGANIZER","ATTENDEE","RECURRENCE-ID","RRULE","RDATE","EXDATE","EXRULE","ATTACH"].iter().any(|key| !values(event,key).is_empty()) || event.items.iter().any(|item| matches!(item,IcalItem::Component(_) | IcalItem::Opaque(_))) { return Err("Scheduled, recurring, attached, alarmed or unparsed event requires an expanded delete preview".into()); }
    let one = |key: &str| -> Result<String, String> {
        let found = values(event,key);
        if found.len() != 1 || found[0].is_empty() || found[0].len() > 4096 || found[0].chars().any(char::is_control) { return Err("Event identity cannot be displayed safely".into()); }
        Ok(found[0].clone())
    };
    Ok((one("UID")?,one("SUMMARY")?))
}

/// An unambiguous contact identity for a delete confirmation.
pub fn preview_contact_delete(raw: &[u8]) -> Result<(String, String), String> {
    if raw.len() > 16 * 1024 * 1024 { return Err("Contact resource exceeds size limit".into()); }
    let card = vcard::tree::cst::VcardCst::parse(raw).map_err(|_| "Invalid contact resource")?;
    if card.begin.is_none() || card.to_bytes() != raw { return Err("Contact must be a single complete vCard".into()); }
    let one = |key: &str| -> Result<String, String> {
        let matching: Vec<_> = card.props.iter().filter(|line| line.name.get().rsplit('.').next().is_some_and(|n| n.eq_ignore_ascii_case(key))).collect();
        if matching.len() != 1 || !matching[0].params.is_empty() { return Err("Contact identity is ambiguous or parameterized".into()); }
        let bytes = matching[0].value.decode_bytes();
        let value = std::str::from_utf8(&bytes).map_err(|_| "Contact identity is not UTF-8")?;
        if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) { return Err("Contact identity cannot be displayed safely".into()); }
        Ok(value.to_owned())
    };
    Ok((one("UID")?,one("FN")?))
}

pub fn resource_index(raw: &[u8]) -> Result<ResourceIndex, String> {
    let tree = parse(raw)?;
    let mut event_uid: Option<String> = None;
    let mut event_master = false;
    let mut event_occurrences = HashSet::new();
    let mut has_task = false;
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        match name(child).as_deref() {
            Some("VTODO") => has_task = true,
            Some("VEVENT") => {
                let ids = values(child,"UID"); let recurrence = values(child,"RECURRENCE-ID");
                if ids.len() != 1 || ids[0].is_empty() || recurrence.len() > 1 || event_uid.as_ref().is_some_and(|old| old != &ids[0]) { return Err("Ambiguous event identity".into()); }
                if let Some(id) = recurrence.first() { if id.is_empty() || !event_occurrences.insert(id.clone()) { return Err("Ambiguous event occurrence".into()); } }
                if recurrence.is_empty() && std::mem::replace(&mut event_master, true) { return Err("Multiple event masters".into()); }
                event_uid = Some(ids[0].clone());
            }
            _ => {}
        }
    }
    match (event_uid, has_task) {
        (Some(uid), false) if event_master => Ok(ResourceIndex::Event {uid}),
        (None, true) => task_index(raw).map(ResourceIndex::Task),
        _ => Err("Unsupported or ambiguous calendar resource".into()),
    }
}

pub fn task_index(raw: &[u8]) -> Result<TaskIndex, String> {
    let tree = parse(raw)?;
    let mut all_uid: Option<String> = None;
    let mut master: Option<&IcalCst<'_>> = None;
    let mut occurrences = HashSet::new();
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        let kind = name(child);
        if kind.as_deref() == Some("VEVENT") { return Err("Mixed event/task resource".into()); }
        if kind.as_deref() != Some("VTODO") { continue; }
        let ids = values(child,"UID"); let recurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0].is_empty() || recurrence.len() > 1 { return Err("Ambiguous task identity".into()); }
        if let Some(id) = recurrence.first() { if id.is_empty() || !occurrences.insert(id.clone()) { return Err("Ambiguous task occurrence".into()); } }
        if all_uid.as_ref().is_some_and(|old| old != &ids[0]) { return Err("Multiple task UIDs in resource".into()); }
        all_uid = Some(ids[0].clone());
        if recurrence.is_empty() {
            if master.replace(child).is_some() { return Err("Multiple task masters".into()); }
        }
    }
    let uid = all_uid.ok_or("Task UID missing")?;
    let master = master.ok_or("Task master missing")?;
    let mut parents = Vec::new();
    for entry in &master.items {
        let IcalItem::Prop(line) = entry else { continue; };
        if !line.name.get().eq_ignore_ascii_case("RELATED-TO") { continue; }
        let params: Vec<_> = line.params.iter().filter(|p| p.name.get().eq_ignore_ascii_case("RELTYPE")).collect();
        if params.len() > 1 || params.first().is_some_and(|p| p.values.len() != 1) { return Err("Ambiguous relation type".into()); }
        let is_parent = params.first().map(|p| p.values[0].get().trim_matches('"').eq_ignore_ascii_case("PARENT")).unwrap_or(true);
        if is_parent { parents.push(line.value.decode().into_owned()); }
    }
    if parents.len() > 1 || parents.first().is_some_and(|p| p.is_empty() || p == &uid) { return Err("Ambiguous task parent".into()); }
    Ok(TaskIndex { uid, parent_uid:parents.pop() })
}

/// Modify a single native master or exception without touching sibling
/// occurrences. The UI must confirm scheduling effects before a DAV write.
pub fn edit_text(raw: &[u8], uid: &str, recurrence_id: Option<&str>, changes: &[(&str, &str)]) -> Result<Vec<u8>, String> {
    if uid.is_empty() || changes.is_empty() { return Err("Missing calendar edit identity or fields".into()); }
    let mut fields = HashSet::new();
    for (key, value) in changes {
        if !matches!(*key, "SUMMARY" | "DESCRIPTION" | "LOCATION") || !fields.insert(*key) || value.len() > 64 * 1024 || value.contains('\0') { return Err("Unsupported or invalid calendar edit".into()); }
    }
    let mut tree = parse(raw)?;
    let mut matches = 0;
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if !matches!(name(child).as_deref(), Some("VEVENT" | "VTODO")) { continue; }
        let ids = values(child,"UID"); let occurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        matches += 1;
        for (key, value) in changes {
            let mut found = 0;
            for field in &mut child.items {
                if let IcalItem::Prop(line) = field {
                    if line.name.get().eq_ignore_ascii_case(key) {
                        found += 1;
                        if found > 1 { return Err("Ambiguous duplicate calendar property".into()); }
                        line.value.set(&[*value]);
                    }
                }
            }
            if found == 0 {
                let mut line = IcalLine::text(*key, "");
                line.value.set(&[*value]);
                child.items.push(IcalItem::Prop(line));
            }
        }
    }
    if matches != 1 { return Err("Calendar target is missing or ambiguous".into()); }
    Ok(tree.to_bytes())
}

/// Replace the start and end of one event using either dates or UTC instants.
/// Custom parameters on the replaced fields require a richer editor and are
/// rejected instead of silently discarding provider-specific information.
pub fn event_time(raw: &[u8], uid: &str, recurrence_id: Option<&str>, start: &str, end: &str, date_only: bool) -> Result<Vec<u8>, String> {
    let valid = |value: &str| {
        let b = value.as_bytes();
        (if date_only { b.len() == 8 } else { b.len() == 16 && b[8] == b'T' && b[15] == b'Z' })
            && ical::recur::IcalRecurDateTime::parse(value).is_ok()
    };
    if uid.is_empty() || !valid(start) || !valid(end) || start >= end { return Err("Invalid event time range".into()); }
    let mut tree = parse(raw)?;
    let mut matches = 0;
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VEVENT") { continue; }
        let ids = values(child, "UID");
        let occurrence = values(child, "RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        matches += 1;
        if !values(child, "DURATION").is_empty() { return Err("Duration-based event requires explicit conversion".into()); }
        for key in ["DTSTART", "DTEND"] {
            if values(child, key).len() > 1 { return Err("Ambiguous event time".into()); }
            for entry in &child.items {
                if let IcalItem::Prop(line) = entry {
                    if line.name.get().eq_ignore_ascii_case(key) && line.params.iter().any(|param| {
                        !param.name.get().eq_ignore_ascii_case("VALUE") || param.values.len() != 1 || !param.values[0].get().eq_ignore_ascii_case("DATE")
                    }) { return Err("Parameterized event time requires a zone-aware editor".into()); }
                }
            }
        }
        child.items.retain(|entry| !matches!(entry, IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case("DTSTART") || line.name.get().eq_ignore_ascii_case("DTEND")));
        for (key, value) in [("DTSTART", start), ("DTEND", end)] {
            let mut line = IcalLine::text(key, value.to_owned());
            if date_only { line.params.push(ical::tree::param::node::IcalParamNode::parse("VALUE=DATE")); }
            child.items.push(IcalItem::Prop(line));
        }
    }
    if matches != 1 { return Err("Event target is missing or ambiguous".into()); }
    Ok(tree.to_bytes())
}

/// Preview a single unscheduled event whose whole effect can be presented.
pub fn preview_event_time(raw: &[u8], uid: &str, start: &str, end: &str, date_only: bool) -> Result<(String, String, Vec<u8>), String> {
    let tree = parse(raw)?;
    if !values(&tree, "METHOD").is_empty() { return Err("Scheduled calendar requires explicit scheduling approval".into()); }
    if tree.items.iter().any(|item| matches!(item,IcalItem::Opaque(_))) { return Err("Calendar contains unparsed data".into()); }
    let mut target = None;
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VEVENT") { continue; }
        if child.items.iter().any(|item| matches!(item,IcalItem::Opaque(_))) { return Err("Event contains unparsed data".into()); }
        if !values(child, "ORGANIZER").is_empty() || !values(child, "ATTENDEE").is_empty() { return Err("Scheduled event requires explicit scheduling approval".into()); }
        let ids = values(child, "UID");
        if ids.len() != 1 || ids[0] != uid { continue; }
        if target.replace(child).is_some() { return Err("Recurring or ambiguous event requires series approval".into()); }
    }
    let target = target.ok_or("Event target is missing")?;
    if ["RECURRENCE-ID", "RRULE", "RDATE", "EXDATE", "EXRULE"].iter().any(|key| !values(target, key).is_empty()) { return Err("Recurring event requires series approval".into()); }
    if target.items.iter().any(|item| matches!(item,IcalItem::Component(_))) { return Err("Alarmed event requires an alarm-effect preview".into()); }
    let one = |key: &str| -> Result<String, String> {
        let found = values(target, key);
        if found.len() != 1 || found[0].len() > 64 || found[0].chars().any(char::is_control) { return Err("Event time cannot be displayed safely".into()); }
        Ok(found[0].clone())
    };
    Ok((one("DTSTART")?, one("DTEND")?, event_time(raw, uid, None, start, end, date_only)?))
}

/// Edit local civil times in an IANA zone. DST gaps and ambiguous folds are
/// rejected; embedded timezone definitions must agree with the IANA instant.
pub fn event_time_zoned(raw: &[u8], uid: &str, recurrence_id: Option<&str>, start: &str, end: &str, tzid: &str) -> Result<Vec<u8>, String> {
    use chrono::{LocalResult, NaiveDateTime, TimeZone};
    if uid.is_empty() || tzid.is_empty() || tzid.len() > 128 || !tzid.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+')) { return Err("Invalid event timezone".into()); }
    let zone: chrono_tz::Tz = tzid.parse().map_err(|_| "Unknown event timezone")?;
    let parse_local = |value: &str| -> Result<(NaiveDateTime, i64), String> {
        if value.len() != 15 { return Err("Invalid local event time".into()); }
        let civil = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").map_err(|_| "Invalid local event time")?;
        match zone.from_local_datetime(&civil) { LocalResult::Single(instant) => Ok((civil,instant.timestamp())), _ => Err("Local event time is ambiguous or nonexistent".into()) }
    };
    let (_, start_utc) = parse_local(start)?;
    let (_, end_utc) = parse_local(end)?;
    if end_utc <= start_utc { return Err("Invalid event time range".into()); }
    let mut tree = parse(raw)?;
    let mut validation = tree.clone();
    for entry in &mut validation.items {
        if let IcalItem::Component(component) = entry {
            if matches!(name(component).as_deref(),Some("VEVENT" | "VTODO")) {
                component.items.retain(|item| !matches!(item, IcalItem::Component(child) if name(child).as_deref() == Some("VALARM")));
                for item in &mut component.items { if let IcalItem::Prop(line) = item { line.params.retain(|p| !p.name.get().eq_ignore_ascii_case("TZID")); } }
            }
        }
    }
    let calendar = validation.decode().validate().map_err(|_| "Calendar timezone cannot be validated")?.into_inner();
    if let Some(embedded) = ical::tz::IcalTz::of_calendar(&calendar, tzid) {
        if embedded.observances.is_empty() { return Err("Calendar timezone has no transitions".into()); }
        for (local, expected) in [(start,start_utc),(end,end_utc)] {
            let civil = ical::recur::IcalRecurDateTime::parse(local).map_err(|_| "Invalid event time")?;
            if embedded.resolve(civil).instant(civil) != Some(expected) { return Err("Embedded timezone differs from selected timezone".into()); }
        }
    }
    let mut matches = 0;
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VEVENT") { continue; }
        let ids = values(child,"UID"); let occurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        matches += 1;
        if !values(child,"DURATION").is_empty() { return Err("Duration-based event requires explicit conversion".into()); }
        for key in ["DTSTART","DTEND"] {
            if values(child,key).len() > 1 { return Err("Ambiguous event time".into()); }
            for entry in &child.items { if let IcalItem::Prop(line) = entry {
                if line.name.get().eq_ignore_ascii_case(key) && (line.params.len() > 1 || line.params.iter().any(|p| !p.name.get().eq_ignore_ascii_case("TZID") || p.values.len() != 1)) { return Err("Event time has unsupported parameters".into()); }
            } }
        }
        child.items.retain(|entry| !matches!(entry, IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case("DTSTART") || line.name.get().eq_ignore_ascii_case("DTEND")));
        for (key,value) in [("DTSTART",start),("DTEND",end)] {
            let mut line = IcalLine::text(key,value.to_owned());
            let mut parameter = ical::tree::param::node::IcalParamNode::parse("TZID=placeholder");
            parameter.values[0].set(tzid.to_owned());
            line.params.push(parameter);
            child.items.push(IcalItem::Prop(line));
        }
    }
    if matches != 1 { return Err("Event target is missing or ambiguous".into()); }
    Ok(tree.to_bytes())
}

/// Zone-aware preview is restricted to one unscheduled, non-recurring event
/// with no alarms; source timezone parameters are returned for display.
pub fn preview_event_time_zoned(raw: &[u8], uid: &str, start: &str, end: &str, tzid: &str) -> Result<(String, Option<String>, String, Option<String>, Vec<u8>), String> {
    let tree = parse(raw)?;
    if !values(&tree,"METHOD").is_empty() || tree.items.iter().any(|item| matches!(item,IcalItem::Opaque(_))) { return Err("Calendar effect cannot be previewed safely".into()); }
    let mut target = None;
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VEVENT") { continue; }
        if !values(child,"ORGANIZER").is_empty() || !values(child,"ATTENDEE").is_empty() { return Err("Scheduled event requires scheduling approval".into()); }
        let ids = values(child,"UID");
        if ids.len() != 1 || ids[0] != uid { continue; }
        if target.replace(child).is_some() { return Err("Recurring event requires series approval".into()); }
    }
    let target = target.ok_or("Event missing")?;
    if ["RECURRENCE-ID","RRULE","RDATE","EXDATE","EXRULE"].iter().any(|key| !values(target,key).is_empty()) || target.items.iter().any(|item| matches!(item,IcalItem::Component(_) | IcalItem::Opaque(_))) { return Err("Recurring or alarmed event requires expanded approval".into()); }
    let field = |key: &str| -> Result<(String,Option<String>), String> {
        let lines: Vec<_> = target.items.iter().filter_map(|item| match item {IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case(key) => Some(line), _ => None}).collect();
        if lines.len() != 1 || lines[0].params.len() > 1 { return Err("Event time is ambiguous".into()); }
        let zone = match lines[0].params.first() {
            None => None,
            Some(param) if param.name.get().eq_ignore_ascii_case("TZID") && param.values.len() == 1 => Some(param.values[0].get().to_owned()),
            _ => return Err("Event time has unsupported parameters".into()),
        };
        let value = lines[0].value.decode().into_owned();
        if value.is_empty() || value.len() > 64 || value.chars().any(char::is_control) || zone.as_ref().is_some_and(|z| z.len() > 128 || z.chars().any(char::is_control)) { return Err("Event time cannot be displayed safely".into()); }
        Ok((value,zone))
    };
    let (old_start,start_zone) = field("DTSTART")?;
    let (old_end,end_zone) = field("DTEND")?;
    Ok((old_start,start_zone,old_end,end_zone,event_time_zoned(raw,uid,None,start,end,tzid)?))
}

/// Create one UTC recurrence override only after the server has expanded and
/// confirmed the occurrence. The original master and existing exceptions stay
/// byte-faithful; inherited properties are copied from the native master.
pub fn create_event_exception(raw: &[u8], expanded: &[u8], uid: &str, recurrence_id: &str, start: &str, end: &str) -> Result<Vec<u8>, String> {
    let valid_utc = |s: &str| s.len() == 16 && s.as_bytes()[8] == b'T' && s.ends_with('Z') && ical::recur::IcalRecurDateTime::parse(s).is_ok();
    if uid.is_empty() || !valid_utc(recurrence_id) || !valid_utc(start) || !valid_utc(end) || start >= end { return Err("Invalid UTC exception".into()); }
    let expansion = parse(expanded)?;
    let mut occurrence_matches = 0;
    for item in &expansion.items {
        let IcalItem::Component(child) = item else { continue; };
        let ids = values(child,"UID");
        if name(child).as_deref() != Some("VEVENT") || ids.len() != 1 || ids[0] != uid { continue; }
        let rid = values(child,"RECURRENCE-ID");
        if rid.len() == 1 && rid[0] == recurrence_id { occurrence_matches += 1; }
    }
    if occurrence_matches != 1 { return Err("Server did not confirm exactly one occurrence".into()); }
    let mut tree = parse(raw)?;
    let mut master = None;
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        let ids = values(child,"UID");
        if name(child).as_deref() != Some("VEVENT") || ids.len() != 1 || ids[0] != uid { continue; }
        let rid = values(child,"RECURRENCE-ID");
        if child.items.iter().any(|item| matches!(item,IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case("RECURRENCE-ID") && line.params.iter().any(|p| p.name.get().eq_ignore_ascii_case("RANGE")))) { return Err("Range override requires explicit series handling".into()); }
        if rid.len() > 1 || rid.first().is_some_and(|value| value == recurrence_id) { return Err("Occurrence already has an override or ambiguous identity".into()); }
        if rid.is_empty() && master.replace(child.clone()).is_some() { return Err("Ambiguous series master".into()); }
    }
    let mut child = master.ok_or("Series master missing")?;
    if values(&child,"RRULE").is_empty() && values(&child,"RDATE").is_empty() { return Err("Event is not recurring".into()); }
    let original_start = values(&child,"DTSTART");
    let original_end = values(&child,"DTEND");
    if original_start.len() != 1 || original_end.len() != 1 || !valid_utc(&original_start[0]) || !valid_utc(&original_end[0]) || !values(&child,"DURATION").is_empty() { return Err("Only UTC date-time series with explicit end are supported".into()); }
    for item in &child.items { if let IcalItem::Prop(line) = item {
        if ["DTSTART","DTEND","RECURRENCE-ID"].iter().any(|key| line.name.get().eq_ignore_ascii_case(key)) && !line.params.is_empty() { return Err("Parameterized time requires zone-aware exception handling".into()); }
    } }
    child.items.retain(|entry| !matches!(entry, IcalItem::Prop(line) if ["RRULE","RDATE","EXDATE","EXRULE","DTSTART","DTEND","RECURRENCE-ID"].iter().any(|key| line.name.get().eq_ignore_ascii_case(key))));
    for (key,value) in [("RECURRENCE-ID",recurrence_id),("DTSTART",start),("DTEND",end)] { child.items.push(IcalItem::Prop(IcalLine::text(key,value.to_owned()))); }
    tree.items.push(IcalItem::Component(child));
    Ok(tree.to_bytes())
}

/// Show one server-expanded occurrence before moving it. Scheduled events and
/// alarms need a richer side-effect preview and are deliberately excluded.
pub fn preview_event_exception(raw: &[u8], expanded: &[u8], uid: &str, recurrence_id: &str, start: &str, end: &str) -> Result<(String, String, String, Vec<u8>), String> {
    let original = parse(raw)?;
    if !values(&original,"METHOD").is_empty() || original.items.iter().any(|item| matches!(item,IcalItem::Opaque(_))) { return Err("Series effect cannot be previewed safely".into()); }
    for item in &original.items { if let IcalItem::Component(child) = item {
        if name(child).as_deref() != Some("VEVENT") { continue; }
        if !values(child,"ORGANIZER").is_empty() || !values(child,"ATTENDEE").is_empty() || child.items.iter().any(|entry| matches!(entry,IcalItem::Component(_) | IcalItem::Opaque(_))) { return Err("Scheduled or alarmed series needs expanded approval".into()); }
    } }
    let expansion = parse(expanded)?;
    let mut selected = None;
    for item in &expansion.items { if let IcalItem::Component(child) = item {
        if name(child).as_deref() != Some("VEVENT") { continue; }
        let ids = values(child,"UID"); let recurrence = values(child,"RECURRENCE-ID");
        if ids.len() == 1 && ids[0] == uid && recurrence.len() == 1 && recurrence[0] == recurrence_id && selected.replace(child).is_some() { return Err("Expanded occurrence is ambiguous".into()); }
    } }
    let selected = selected.ok_or("Expanded occurrence missing")?;
    let one = |key: &str| -> Result<String,String> {
        let found = values(selected,key);
        if found.len() != 1 || found[0].is_empty() || found[0].len() > 4096 || found[0].chars().any(char::is_control) { return Err("Occurrence cannot be displayed safely".into()); }
        Ok(found[0].clone())
    };
    Ok((one("SUMMARY")?,one("DTSTART")?,one("DTEND")?,create_event_exception(raw,expanded,uid,recurrence_id,start,end)?))
}

/// Preview a single text edit only when the whole resource has no scheduling
/// recipients or series-wide effect. The original raw resource is retained.
pub fn preview_text_edit(raw: &[u8], uid: &str, recurrence_id: Option<&str>, field: &str, value: &str) -> Result<(Option<String>, Vec<u8>), String> {
    if !matches!(field, "SUMMARY" | "DESCRIPTION") || value.len() > 4096 || value.chars().any(char::is_control) { return Err("Text preview cannot be displayed safely".into()); }
    let tree = parse(raw)?;
    if !values(&tree,"METHOD").is_empty() { return Err("Scheduled calendar requires explicit scheduling approval".into()); }
    let mut target = None;
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if !matches!(name(child).as_deref(),Some("VEVENT" | "VTODO")) { continue; }
        if !values(child,"ORGANIZER").is_empty() || !values(child,"ATTENDEE").is_empty() { return Err("Scheduled resource requires explicit scheduling approval".into()); }
        let ids = values(child,"UID"); let occurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        if target.replace(child).is_some() { return Err("Calendar target is ambiguous".into()); }
    }
    let target = target.ok_or("Calendar target is missing")?;
    if recurrence_id.is_none() && (!values(target,"RRULE").is_empty() || !values(target,"RDATE").is_empty()) { return Err("Recurring master requires series approval".into()); }
    for item in &target.items {
        if let IcalItem::Prop(line) = item {
            if line.name.get().eq_ignore_ascii_case(field) && !line.params.is_empty() { return Err("Parameterized text preview is unsupported".into()); }
        }
    }
    let old = values(target,field);
    if old.len() > 1 || old.first().is_some_and(|v| v.len() > 4096 || v.chars().any(char::is_control)) { return Err("Text preview cannot be displayed safely".into()); }
    Ok((old.into_iter().next(), edit_text(raw,uid,recurrence_id,&[(field,value)])?))
}

/// Create a personal recovery projection. Mapping must be generated and
/// persisted before any dispatch; exceptions of a series share one replacement
/// UID. Unmapped parents fail closed rather than linking to an old resource.
pub fn recovered_calendar(raw: &[u8], uid_map: &HashMap<String, String>) -> Result<Vec<u8>, String> {
    if uid_map.is_empty() || uid_map.values().any(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control) || uid_map.contains_key(s)) || uid_map.values().collect::<HashSet<_>>().len() != uid_map.len() { return Err("Invalid recovery UID mapping".into()); }
    let mut tree = parse(raw)?;
    tree.items.retain(|item| !matches!(item, IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case("METHOD")));
    let mut found = HashSet::new();
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() == Some("VTIMEZONE") { continue; }
        if !matches!(name(child).as_deref(), Some("VEVENT" | "VTODO")) { return Err("Unsupported recovered calendar component".into()); }
        if child.items.iter().any(|entry| matches!(entry, IcalItem::Component(nested) if name(nested).as_deref() != Some("VALARM"))) { return Err("Unsupported nested recovered component".into()); }
        let ids = values(child, "UID");
        if ids.len() != 1 { return Err("Recovered component must have one UID".into()); }
        let replacement = uid_map.get(&ids[0]).ok_or("Missing recovery UID mapping")?;
        found.insert(ids[0].clone());
        child.items.retain(|entry| match entry {
            IcalItem::Prop(line) => !line.name.get().eq_ignore_ascii_case("ORGANIZER") && !line.name.get().eq_ignore_ascii_case("ATTENDEE"),
            IcalItem::Component(nested) => name(nested).as_deref() != Some("VALARM"),
            IcalItem::Opaque(_) => false,
        });
        for entry in &mut child.items {
            if let IcalItem::Prop(line) = entry {
                if line.name.get().eq_ignore_ascii_case("UID") { line.value.set(&[replacement]); }
                if line.name.get().eq_ignore_ascii_case("RELATED-TO") {
                    let original = line.value.decode().into_owned();
                    let mapped = uid_map.get(&original).ok_or("Missing recovery parent mapping")?;
                    line.value.set(&[mapped]);
                }
            }
        }
    }
    if found.is_empty() { return Err("Recovery calendar contains no events or tasks".into()); }
    Ok(tree.to_bytes())
}

/// Create a new contact identity while preserving all other original fields,
/// including embedded photos. Original bytes remain with the recovery caller.
pub fn recovered_contact(raw: &[u8], replacement_uid: &str) -> Result<Vec<u8>, String> {
    if raw.len() > 16 * 1024 * 1024 || replacement_uid.is_empty() || replacement_uid.len() > 4096 || replacement_uid.chars().any(char::is_control) { return Err("Invalid recovery contact UID".into()); }
    let mut card = vcard::tree::cst::VcardCst::parse(raw).map_err(|_| "Invalid contact resource")?;
    if card.begin.is_none() || card.to_bytes() != raw { return Err("Contact must be a single complete vCard".into()); }
    let mut matches = card.props.iter_mut().filter(|line| line.name.get().rsplit('.').next().is_some_and(|name| name.eq_ignore_ascii_case("UID")));
    let uid = matches.next().ok_or("Contact UID missing")?;
    if matches.next().is_some() || !uid.name.get().eq_ignore_ascii_case("UID") || !uid.params.is_empty() { return Err("Contact UID is ambiguous or parameterized".into()); }
    if uid.value.decode_bytes().as_ref() == replacement_uid.as_bytes() { return Err("Recovery contact UID must be new".into()); }
    uid.value.set(&[replacement_uid]);
    Ok(card.to_bytes())
}

/// Edit one selected contact text property while retaining every other wire
/// line, including photos and vendor extensions. `index` disambiguates repeated
/// properties. Non-UTF-8/transfer-encoded target fields are rejected rather than
/// rewriting them with an incorrect charset.
pub fn edit_contact_text(raw: &[u8], key: &str, index: usize, value: &str) -> Result<Vec<u8>, String> {
    if raw.len() > 16 * 1024 * 1024 || !matches!(key, "FN" | "NOTE" | "TITLE" | "ROLE" | "EMAIL") || value.len() > 64 * 1024 || value.contains('\0') || key == "EMAIL" && (value.is_empty() || value.chars().any(char::is_control)) { return Err("Unsupported or invalid contact edit".into()); }
    let mut card = vcard::tree::cst::VcardCst::parse(raw).map_err(|_| "Invalid contact resource")?;
    if card.begin.is_none() || card.to_bytes() != raw { return Err("Contact must be a single complete vCard".into()); }
    let target = card.props.iter_mut().filter(|line| line.name.get().rsplit('.').next().is_some_and(|name| name.eq_ignore_ascii_case(key))).nth(index).ok_or("Contact property is missing")?;
    if target.params.iter().any(|param| param.name.get().eq_ignore_ascii_case("CHARSET") || param.name.get().eq_ignore_ascii_case("ENCODING") || param.name.get().eq_ignore_ascii_case("VALUE") && (param.values.len() != 1 || !param.values[0].get().eq_ignore_ascii_case("TEXT"))) { return Err("Encoded or non-text contact field cannot be safely edited".into()); }
    target.value.set(&[value]);
    Ok(card.to_bytes())
}

pub struct ContactFieldPreview {
    pub version: String,
    pub property_name: String,
    pub parameters: Vec<(String, Vec<String>)>,
    pub value_type: String,
    pub old_value: String,
    pub new_value: String,
    pub raw: Vec<u8>,
}

/// Edit one existing EMAIL, TEL, or first ORG component while retaining
/// group, parameters, remaining organization units and all unrelated data.
pub fn preview_contact_field(raw: &[u8], key: &str, index: usize, value: &str) -> Result<ContactFieldPreview, String> {
    if raw.len() > 16 * 1024 * 1024 || !matches!(key,"EMAIL" | "TEL" | "ORG") || value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) { return Err("Invalid contact field edit".into()); }
    let mut card = vcard::tree::cst::VcardCst::parse(raw).map_err(|_| "Invalid contact resource")?;
    if card.begin.is_none() || card.to_bytes() != raw { return Err("Contact must be a single complete vCard".into()); }
    let versions: Vec<_> = card.props.iter().filter(|line| line.name.get().eq_ignore_ascii_case("VERSION")).collect();
    if versions.len() != 1 || !versions[0].params.is_empty() { return Err("Contact version is ambiguous".into()); }
    let version = versions[0].value.decode().into_owned();
    if !matches!(version.as_str(),"3.0" | "4.0") { return Err("Contact version is unsupported".into()); }
    // jCard moves the first group prefix to the group parameter; match that
    // same property-name rule so a UI list index selects this exact line.
    let target = card.props.iter_mut().filter(|line| line.name.get().split_once('.').map(|(_,name)| name).unwrap_or_else(|| line.name.get()).eq_ignore_ascii_case(key)).nth(index).ok_or("Contact property missing")?;
    if target.params.iter().any(|p| p.name.get().eq_ignore_ascii_case("CHARSET") || p.name.get().eq_ignore_ascii_case("ENCODING")) { return Err("Encoded contact property is unsupported".into()); }
    let value_params: Vec<_> = target.params.iter().filter(|p| p.name.get().eq_ignore_ascii_case("VALUE")).collect();
    if value_params.len() > 1 || value_params.first().is_some_and(|p| p.values.len() != 1) { return Err("Contact value type is ambiguous".into()); }
    let value_type = value_params.first().map(|p| p.values[0].get().to_ascii_lowercase()).unwrap_or_else(|| "text".into());
    if value_type != "text" && !(key == "TEL" && value_type == "uri") { return Err("Contact value type is unsupported".into()); }
    if value_type == "uri" && (!value.starts_with("tel:") || value.len() == 4 || !value[4..].bytes().all(|b| b.is_ascii_digit() || matches!(b,b'+' | b'-' | b'.' | b'(' | b')'))) { return Err("Telephone URI requires a simple tel number".into()); }
    let property_name = target.name.get().to_owned();
    let parameters = target.params.iter().map(|p| (p.name.get().to_owned(),p.values.iter().map(|v| v.get().to_owned()).collect())).collect();
    let decoded = target.value.decode_bytes().into_owned();
    std::str::from_utf8(&decoded).map_err(|_| "Contact field is not UTF-8")?;
    let old_value = if key == "ORG" {
        let first = target.value.decode_component_list(0);
        if first.len() != 1 { return Err("Organization component is ambiguous".into()); }
        first[0].to_string()
    } else {
        std::str::from_utf8(&decoded).map_err(|_| "Contact field is not UTF-8")?.to_owned()
    };
    if old_value.len() > 4096 || old_value.chars().any(char::is_control) { return Err("Contact field cannot be displayed safely".into()); }
    if key == "ORG" { target.value.set_component(0,&[value]); } else { target.value.set(&[value]); }
    Ok(ContactFieldPreview {version,property_name,parameters,value_type,old_value,new_value:value.into(),raw:card.to_bytes()})
}

/// Append one typed contact field without rewriting existing fields.
pub fn preview_contact_add(raw: &[u8], key: &str, value: &str, value_type: &str) -> Result<ContactFieldPreview, String> {
    if raw.len() > 16 * 1024 * 1024 || !matches!(key,"EMAIL" | "TEL" | "ORG") || value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) || !matches!(value_type,"text" | "uri") || value_type == "uri" && key != "TEL" { return Err("Invalid new contact field".into()); }
    if value_type == "uri" && (!value.starts_with("tel:") || value.len() == 4 || !value[4..].bytes().all(|b| b.is_ascii_digit() || matches!(b,b'+' | b'-' | b'.' | b'(' | b')'))) { return Err("Telephone URI requires a simple tel number".into()); }
    let mut card = vcard::tree::cst::VcardCst::parse(raw).map_err(|_| "Invalid contact resource")?;
    if card.begin.is_none() || card.to_bytes() != raw { return Err("Contact must be a single complete vCard".into()); }
    let versions: Vec<_> = card.props.iter().filter(|line| line.name.get().eq_ignore_ascii_case("VERSION")).collect();
    if versions.len() != 1 || !versions[0].params.is_empty() { return Err("Contact version is ambiguous".into()); }
    let version = versions[0].value.decode().into_owned();
    if !matches!(version.as_str(),"3.0" | "4.0") || value_type == "uri" && version != "4.0" { return Err("Contact version or value type is unsupported".into()); }
    let mut line = vcard::tree::line::VcardLine::text(key.to_owned(), "");
    if value_type == "uri" { line.params.push(vcard::tree::param::node::VcardParamNode::parse("VALUE=uri")); }
    line.value.set(&[value]);
    card.props.push(line);
    let parameters = if value_type == "uri" {vec![("VALUE".into(),vec!["uri".into()])]} else {Vec::new()};
    Ok(ContactFieldPreview {version,property_name:key.into(),parameters,value_type:value_type.into(),old_value:String::new(),new_value:value.into(),raw:card.to_bytes()})
}

/// Remove exactly one selected field, preserving its group and parameters in
/// the preview and retaining all other raw contact lines.
pub fn preview_contact_remove(raw: &[u8], key: &str, index: usize) -> Result<ContactFieldPreview, String> {
    let mut preview = preview_contact_field(raw,key,index,"tel:+1")?;
    let mut card = vcard::tree::cst::VcardCst::parse(raw).map_err(|_| "Invalid contact resource")?;
    let position = card.props.iter().enumerate().filter(|(_,line)| line.name.get().split_once('.').map(|(_,name)| name).unwrap_or_else(|| line.name.get()).eq_ignore_ascii_case(key)).nth(index).map(|(position,_)| position).ok_or("Contact property missing")?;
    if key == "ORG" {
        let bytes = card.props[position].value.decode_bytes();
        let value = std::str::from_utf8(&bytes).map_err(|_| "Contact field is not UTF-8")?;
        if value.len() > 4096 || value.chars().any(char::is_control) { return Err("Contact field cannot be displayed safely".into()); }
        preview.old_value = value.to_owned();
    }
    card.props.remove(position);
    preview.new_value.clear();
    preview.raw = card.to_bytes();
    Ok(preview)
}

/// Replace or add a single vCard 4 URI photo. The caller constructs and
/// validates the bounded data URI; non-URI/ambiguous existing photos stay intact.
pub fn contact_photo(raw: &[u8], uri: &str) -> Result<(Option<Vec<u8>>, Vec<u8>), String> {
    let encoded = uri.strip_prefix("data:image/png;base64,").or_else(|| uri.strip_prefix("data:image/jpeg;base64,")).ok_or("Invalid photo URI")?;
    if raw.len() > 16 * 1024 * 1024 || uri.len() > 3 * 1024 * 1024 || encoded.is_empty() || !encoded.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b,b'+' | b'/' | b'=')) { return Err("Invalid photo URI".into()); }
    let mut card = vcard::tree::cst::VcardCst::parse(raw).map_err(|_| "Invalid contact resource")?;
    if card.begin.is_none() || card.to_bytes() != raw { return Err("Contact must be a single complete vCard".into()); }
    let versions: Vec<_> = card.props.iter().filter(|line| line.name.get().eq_ignore_ascii_case("VERSION")).collect();
    if versions.len() != 1 || versions[0].value.decode().as_ref() != "4.0" || !versions[0].params.is_empty() { return Err("Photo editing requires vCard 4.0".into()); }
    let indexes: Vec<_> = card.props.iter().enumerate().filter(|(_,line)| line.name.get().split_once('.').map(|(_,name)| name).unwrap_or_else(|| line.name.get()).eq_ignore_ascii_case("PHOTO")).map(|(i,_)| i).collect();
    if indexes.len() > 1 { return Err("Contact has multiple photos".into()); }
    let replacement = vcard::tree::line::VcardLine::text("PHOTO",uri.to_owned());
    let old = if let Some(index) = indexes.first() {
        let target = &mut card.props[*index];
        if target.params.iter().any(|p| !p.name.get().eq_ignore_ascii_case("VALUE") || p.values.len() != 1 || !p.values[0].get().eq_ignore_ascii_case("URI")) || target.params.len() > 1 { return Err("Existing photo is not a simple URI".into()); }
        let old = target.value.decode_bytes().into_owned();
        target.value = replacement.value;
        Some(old)
    } else {
        card.props.push(replacement);
        None
    };
    Ok((old,card.to_bytes()))
}

/// Prepare the narrowly scoped, unambiguous formatted-name change used by
/// the human confirmation flow. Both the displayed old name and proposed bytes
/// come from the same parsed resource; parameterized names require a richer UI.
pub fn edit_unique_contact_name(raw: &[u8], value: &str) -> Result<(String, Vec<u8>), String> {
    if raw.len() > 16 * 1024 * 1024 || value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return Err("Invalid contact name".into());
    }
    let mut card = vcard::tree::cst::VcardCst::parse(raw).map_err(|_| "Invalid contact resource")?;
    if card.begin.is_none() || card.to_bytes() != raw { return Err("Contact must be a single complete vCard".into()); }
    let mut matches = card.props.iter_mut().filter(|line| line.name.get().rsplit('.').next().is_some_and(|name| name.eq_ignore_ascii_case("FN")));
    let target = matches.next().ok_or("Contact name is missing")?;
    if matches.next().is_some() || !target.name.get().eq_ignore_ascii_case("FN") || !target.params.is_empty() { return Err("Contact name is ambiguous or parameterized".into()); }
    let old_bytes = target.value.decode_bytes();
    let old = std::str::from_utf8(&old_bytes).map_err(|_| "Contact name is not UTF-8")?.to_owned();
    if old.is_empty() || old.len() > 4096 || old.chars().any(char::is_control) { return Err("Contact name cannot be displayed safely".into()); }
    target.value.set(&[value]);
    Ok((old, card.to_bytes()))
}

/// Capture a UTC completion timestamp once for an effect-bound approval.
pub fn current_task_completion_time() -> Result<String, String> {
    use chrono::Datelike;
    let now = chrono::Utc::now();
    if !(0..=9999).contains(&now.year()) { return Err("System clock is outside the supported calendar range".into()); }
    Ok(now.format("%Y%m%dT%H%M%SZ").to_string())
}

/// Complete or reopen one VTODO. The timestamp is explicitly supplied so the
/// approved effect remains stable across retries and is recorded before send.
pub fn task_completion(raw: &[u8], uid: &str, recurrence_id: Option<&str>, completed_at: Option<&str>) -> Result<Vec<u8>, String> {
    if uid.is_empty() { return Err("Missing task UID".into()); }
    if let Some(value) = completed_at {
        let b = value.as_bytes();
        if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' || ical::recur::IcalRecurDateTime::parse(value).is_err() { return Err("Completion timestamp must be UTC".into()); }
    }
    let mut tree = parse(raw)?;
    let mut matches = 0;
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VTODO") { continue; }
        let ids = values(child,"UID"); let occurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        matches += 1;
        for key in ["STATUS", "PERCENT-COMPLETE", "COMPLETED"] {
            if values(child,key).len() > 1 { return Err("Ambiguous duplicate task property".into()); }
        }
        child.items.retain(|entry| !matches!(entry, IcalItem::Prop(line) if ["STATUS", "PERCENT-COMPLETE", "COMPLETED"].iter().any(|key| line.name.get().eq_ignore_ascii_case(key))));
        let (state, percent) = if completed_at.is_some() { ("COMPLETED", "100") } else { ("NEEDS-ACTION", "0") };
        child.items.push(IcalItem::Prop(IcalLine::text("STATUS", state)));
        child.items.push(IcalItem::Prop(IcalLine::text("PERCENT-COMPLETE", percent)));
        if let Some(time) = completed_at { child.items.push(IcalItem::Prop(IcalLine::text("COMPLETED", time.to_owned()))); }
    }
    if matches != 1 { return Err("Task target is missing or ambiguous".into()); }
    Ok(tree.to_bytes())
}

/// Preview and prepare one unambiguous VTODO without scheduling recipients.
/// Series masters are excluded until their broader effect can be displayed.
pub fn preview_task_completion(raw: &[u8], uid: &str, recurrence_id: Option<&str>, completed_at: Option<&str>) -> Result<(String, Option<String>, Option<String>, Option<String>, Vec<u8>), String> {
    let tree = parse(raw)?;
    if !values(&tree,"METHOD").is_empty() { return Err("Scheduled calendar requires explicit scheduling approval".into()); }
    let mut target = None;
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VTODO") { continue; }
        if !values(child,"ORGANIZER").is_empty() || !values(child,"ATTENDEE").is_empty() { return Err("Scheduled task requires explicit scheduling approval".into()); }
        let ids = values(child,"UID");
        let occurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        if target.is_some() { return Err("Task target is ambiguous".into()); }
        target = Some(child);
    }
    let target = target.ok_or("Task target is missing")?;
    if !values(target,"ORGANIZER").is_empty() || !values(target,"ATTENDEE").is_empty() { return Err("Scheduled task requires explicit scheduling approval".into()); }
    if target.items.iter().any(|item| matches!(item,IcalItem::Component(_) | IcalItem::Opaque(_))) { return Err("Task alarms or unparsed data require an expanded preview".into()); }
    if recurrence_id.is_none() && (!values(target,"RRULE").is_empty() || !values(target,"RDATE").is_empty()) { return Err("Recurring task master requires series approval".into()); }
    for item in &target.items {
        if let IcalItem::Prop(line) = item {
            if ["SUMMARY","STATUS","PERCENT-COMPLETE","COMPLETED","RECURRENCE-ID"].iter().any(|key| line.name.get().eq_ignore_ascii_case(key)) && !line.params.is_empty() { return Err("Parameterized task preview is unsupported".into()); }
        }
    }
    let single = |key: &str| -> Result<Option<String>, String> {
        let found = values(target,key);
        if found.len() > 1 { return Err("Ambiguous task preview".into()); }
        let value = found.into_iter().next();
        if value.as_ref().is_some_and(|v| v.len() > 4096 || v.chars().any(char::is_control)) { return Err("Task preview cannot be displayed safely".into()); }
        Ok(value)
    };
    let summary = single("SUMMARY")?.filter(|s| !s.is_empty()).ok_or("Task summary is missing")?;
    let status = single("STATUS")?;
    let completed = single("COMPLETED")?;
    let percent = single("PERCENT-COMPLETE")?;
    let edited = task_completion(raw,uid,recurrence_id,completed_at)?;
    Ok((summary,status,completed,percent,edited))
}

/// Prepare one attendee's explicit RSVP. Other attendee parameters and unknown
/// properties remain intact; callers must confirm scheduling side effects.
pub fn respond_attendee(raw: &[u8], uid: &str, recurrence_id: Option<&str>, attendee: &str, response: &str) -> Result<Vec<u8>, String> {
    let encoded = match response { "accept" => "ACCEPTED", "decline" => "DECLINED", "tentative" => "TENTATIVE", _ => return Err("Unsupported RSVP".into()) };
    if uid.is_empty() || attendee.is_empty() || attendee.contains(['\r','\n']) { return Err("Invalid RSVP identity".into()); }
    let mut tree = parse(raw)?; let mut targets = 0; let mut attendees = 0;
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VEVENT") { continue; }
        let ids = values(child,"UID"); let occurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        targets += 1;
        for item in &mut child.items {
            let IcalItem::Prop(line) = item else { continue; };
            if !line.name.get().eq_ignore_ascii_case("ATTENDEE") || line.value.decode().as_ref() != attendee { continue; }
            attendees += 1;
            let params: Vec<_> = line.params.iter_mut().filter(|p| p.name.get().eq_ignore_ascii_case("PARTSTAT")).collect();
            if params.len() > 1 { return Err("Ambiguous attendee response".into()); }
            if let Some(param) = params.into_iter().next() {
                if param.values.len() != 1 { return Err("Ambiguous attendee response".into()); }
                param.values[0].set(encoded);
            } else {
                let parameter = match response { "accept" => "PARTSTAT=ACCEPTED", "decline" => "PARTSTAT=DECLINED", _ => "PARTSTAT=TENTATIVE" };
                line.params.push(ical::tree::param::node::IcalParamNode::parse(parameter));
            }
        }
    }
    if targets != 1 || attendees != 1 { return Err("RSVP target is missing or ambiguous".into()); }
    Ok(tree.to_bytes())
}

/// A single invitation response preview for an already verified identity.
pub fn preview_attendee_response(raw: &[u8], uid: &str, recurrence_id: Option<&str>, attendee: &str, response: &str) -> Result<(String, String, Vec<u8>), String> {
    let tree = parse(raw)?;
    let mut selected = None;
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VEVENT") { continue; }
        let ids = values(child,"UID"); let occurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        if selected.replace(child).is_some() { return Err("Ambiguous invitation".into()); }
    }
    let selected = selected.ok_or("Invitation missing")?;
    if recurrence_id.is_none() && (!values(selected,"RRULE").is_empty() || !values(selected,"RDATE").is_empty()) { return Err("Recurring invitation requires explicit series approval".into()); }
    if selected.items.iter().any(|item| matches!(item, IcalItem::Opaque(_)) || matches!(item, IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case("RECURRENCE-ID") && line.params.iter().any(|p| p.name.get().eq_ignore_ascii_case("RANGE")))) { return Err("Invitation effect is not limited to one understood occurrence".into()); }
    let organizers = values(selected,"ORGANIZER");
    if organizers.len() != 1 || organizers[0].is_empty() || organizers[0].len() > 4096 || organizers[0].chars().any(char::is_control) { return Err("Invitation organizer missing or ambiguous".into()); }
    let mut old = None;
    for item in &selected.items {
        let IcalItem::Prop(line) = item else { continue; };
        if !line.name.get().eq_ignore_ascii_case("ATTENDEE") || line.value.decode().as_ref() != attendee { continue; }
        let params: Vec<_> = line.params.iter().filter(|p| p.name.get().eq_ignore_ascii_case("PARTSTAT")).collect();
        if params.len() > 1 || params.first().is_some_and(|p| p.values.len() != 1) || old.is_some() { return Err("Ambiguous invitation response".into()); }
        old = Some(params.first().map(|p| p.values[0].get().to_owned()).unwrap_or_else(|| "NEEDS-ACTION".into()));
    }
    let old = old.ok_or("Invitation attendee missing")?;
    if old.len() > 128 || old.chars().any(char::is_control) { return Err("Invalid invitation response".into()); }
    Ok((organizers[0].clone(),old,respond_attendee(raw,uid,recurrence_id,attendee,response)?))
}

/// Prepare cancellation of exactly one event master or existing exception.
/// The caller must confirm recipients and server scheduling side effects.
pub fn cancel_event(raw: &[u8], uid: &str, recurrence_id: Option<&str>, sequence: u32) -> Result<Vec<u8>, String> {
    if uid.is_empty() || sequence > i32::MAX as u32 { return Err("Invalid event UID or sequence".into()); }
    let mut tree = parse(raw)?; let mut matches = 0;
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VEVENT") { continue; }
        let ids = values(child,"UID"); let occurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        matches += 1;
        let previous = values(child,"SEQUENCE");
        if previous.len() > 1 || values(child,"STATUS").len() > 1 { return Err("Ambiguous event sequence or status".into()); }
        let previous = match previous.first() { Some(value) => value.parse::<u32>().map_err(|_| "Invalid event sequence")?, None => 0 };
        if sequence <= previous { return Err("Cancellation sequence must advance".into()); }
        child.items.retain(|entry| !matches!(entry, IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case("STATUS") || line.name.get().eq_ignore_ascii_case("SEQUENCE")));
        child.items.push(IcalItem::Prop(IcalLine::text("STATUS", "CANCELLED")));
        child.items.push(IcalItem::Prop(IcalLine::text("SEQUENCE", sequence.to_string())));
    }
    if matches != 1 { return Err("Event target is missing or ambiguous".into()); }
    Ok(tree.to_bytes())
}

pub struct EventCancellationPreview {
    pub summary: String,
    pub organizer: String,
    pub attendees: Vec<String>,
    pub old_sequence: u32,
    pub new_sequence: u32,
    pub has_alarms: bool,
    pub raw: Vec<u8>,
}

/// Preview cancellation of one non-series event or an existing single
/// exception; a recurring master needs a complete series/recipient preview.
pub fn preview_event_cancellation(raw: &[u8], uid: &str, recurrence_id: Option<&str>, sequence: u32) -> Result<EventCancellationPreview, String> {
    let tree = parse(raw)?;
    if !values(&tree,"METHOD").is_empty() || tree.items.iter().any(|item| matches!(item,IcalItem::Opaque(_))) { return Err("Cancellation effect cannot be previewed safely".into()); }
    let mut target = None;
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VEVENT") { continue; }
        let ids = values(child,"UID"); let occurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        if target.replace(child).is_some() { return Err("Cancellation target is ambiguous".into()); }
    }
    let target = target.ok_or("Cancellation target missing")?;
    if recurrence_id.is_none() && (!values(target,"RRULE").is_empty() || !values(target,"RDATE").is_empty()) { return Err("Series cancellation requires a complete series preview".into()); }
    if recurrence_id.is_none() && tree.items.iter().filter(|item| matches!(item,IcalItem::Component(child) if name(child).as_deref() == Some("VEVENT"))).count() != 1 { return Err("Multiple occurrences require a series preview".into()); }
    if target.items.iter().any(|item| matches!(item,IcalItem::Opaque(_)) || matches!(item,IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case("RECURRENCE-ID") && line.params.iter().any(|p| p.name.get().eq_ignore_ascii_case("RANGE")))) { return Err("Cancellation effect is not a single understood occurrence".into()); }
    let single = |key: &str| -> Result<String, String> {
        let found = values(target,key);
        if found.len() != 1 || found[0].is_empty() || found[0].len() > 4096 || found[0].chars().any(char::is_control) { return Err("Cancellation identity cannot be displayed safely".into()); }
        Ok(found[0].clone())
    };
    let organizer = single("ORGANIZER")?; let summary = single("SUMMARY")?;
    let attendees = values(target,"ATTENDEE");
    if attendees.is_empty() || attendees.len() > 512 || attendees.iter().any(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control)) || attendees.iter().collect::<HashSet<_>>().len() != attendees.len() { return Err("Cancellation recipients are missing or ambiguous".into()); }
    for item in &target.items { if let IcalItem::Prop(line) = item {
        if line.name.get().eq_ignore_ascii_case("ORGANIZER") && line.params.iter().any(|p| !p.name.get().eq_ignore_ascii_case("CN")) { return Err("Organizer delegation requires expanded approval".into()); }
        if line.name.get().eq_ignore_ascii_case("ATTENDEE") && line.params.iter().any(|p| !["CN","ROLE","PARTSTAT","RSVP","CUTYPE"].iter().any(|key| p.name.get().eq_ignore_ascii_case(key))) { return Err("Attendee delivery parameters require expanded approval".into()); }
    } }
    let old = values(target,"SEQUENCE");
    if old.len() > 1 { return Err("Cancellation sequence is ambiguous".into()); }
    let old_sequence = match old.first() {Some(value) => value.parse::<u32>().map_err(|_| "Invalid sequence")?,None => 0};
    let has_alarms = target.items.iter().any(|item| matches!(item,IcalItem::Component(_)));
    Ok(EventCancellationPreview {summary,organizer,attendees,old_sequence,new_sequence:sequence,has_alarms,raw:cancel_event(raw,uid,recurrence_id,sequence)?})
}

/// Change priority and an optional all-day or UTC due value on a selected task.
/// Local time and custom-zone changes are intentionally rejected here rather
/// than silently converting them; they need an explicit zone-aware editor.
pub fn task_attributes(raw: &[u8], uid: &str, recurrence_id: Option<&str>, priority: Option<u8>, due: Option<(&str, bool)>) -> Result<Vec<u8>, String> {
    if uid.is_empty() || priority.is_none() && due.is_none() || priority.is_some_and(|v| v > 9) { return Err("Invalid task attributes".into()); }
    if let Some((value, date_only)) = due {
        let b = value.as_bytes();
        let valid = (if date_only { b.len() == 8 } else { b.len() == 16 && b[8] == b'T' && b[15] == b'Z' }) && ical::recur::IcalRecurDateTime::parse(value).is_ok();
        if !valid { return Err("Task due date must be an all-day date or UTC timestamp".into()); }
    }
    let mut tree = parse(raw)?; let mut matches = 0;
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VTODO") { continue; }
        let ids = values(child,"UID"); let occurrence = values(child,"RECURRENCE-ID");
        if ids.len() != 1 || ids[0] != uid || occurrence.len() > 1 || occurrence.first().map(String::as_str) != recurrence_id { continue; }
        matches += 1;
        if priority.is_some() && values(child,"PRIORITY").len() > 1 || due.is_some() && values(child,"DUE").len() > 1 { return Err("Ambiguous duplicate task property".into()); }
        child.items.retain(|entry| !matches!(entry, IcalItem::Prop(line) if priority.is_some() && line.name.get().eq_ignore_ascii_case("PRIORITY") || due.is_some() && line.name.get().eq_ignore_ascii_case("DUE")));
        if let Some(value) = priority { child.items.push(IcalItem::Prop(IcalLine::text("PRIORITY", value.to_string()))); }
        if let Some((value, date_only)) = due {
            let mut line = IcalLine::text("DUE", value.to_owned());
            if date_only { line.params.push(ical::tree::param::node::IcalParamNode::parse("VALUE=DATE")); }
            child.items.push(IcalItem::Prop(line));
        }
    }
    if matches != 1 { return Err("Task target is missing or ambiguous".into()); }
    Ok(tree.to_bytes())
}

/// Preview task priority/due changes only when no scheduling or series-wide
/// side effect is present. Existing due parameters are exposed separately.
pub fn preview_task_attributes(raw: &[u8], uid: &str, priority: Option<u8>, due: Option<(&str, bool)>) -> Result<(String, Option<String>, Option<String>, Vec<u8>), String> {
    let tree = parse(raw)?;
    if !values(&tree,"METHOD").is_empty() || tree.items.iter().any(|item| matches!(item,IcalItem::Opaque(_))) { return Err("Task effect cannot be previewed safely".into()); }
    let mut target = None;
    for item in &tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VTODO") { continue; }
        if !values(child,"ORGANIZER").is_empty() || !values(child,"ATTENDEE").is_empty() { return Err("Scheduled task requires scheduling approval".into()); }
        let ids = values(child,"UID");
        if ids.len() != 1 || ids[0] != uid { continue; }
        if target.replace(child).is_some() { return Err("Recurring task requires series approval".into()); }
    }
    let target = target.ok_or("Task missing")?;
    if ["RECURRENCE-ID","RRULE","RDATE","EXDATE","EXRULE"].iter().any(|key| !values(target,key).is_empty()) || target.items.iter().any(|item| matches!(item,IcalItem::Opaque(_) | IcalItem::Component(_))) { return Err("Recurring or alarmed task requires expanded approval".into()); }
    for item in &target.items { if let IcalItem::Prop(line) = item {
        if ["SUMMARY","PRIORITY","DUE"].iter().any(|key| line.name.get().eq_ignore_ascii_case(key)) && !line.params.is_empty() { return Err("Parameterized task fields require a richer preview".into()); }
    } }
    let one = |key: &str| -> Result<Option<String>, String> {
        let found = values(target,key);
        if found.len() > 1 || found.first().is_some_and(|v| v.len() > 4096 || v.chars().any(char::is_control)) { return Err("Task field cannot be displayed safely".into()); }
        Ok(found.into_iter().next())
    };
    let name = one("SUMMARY")?.filter(|value| !value.is_empty()).ok_or("Task summary missing")?;
    Ok((name,one("PRIORITY")?,one("DUE")?,task_attributes(raw,uid,None,priority,due)?))
}

/// Set or remove a native VTODO parent UID. Caller must validate that the
/// selected target belongs to the account and that the resulting graph is
/// acyclic before requesting confirmation.
pub fn task_parent(raw: &[u8], uid: &str, parent: Option<&str>) -> Result<Vec<u8>, String> {
    if uid.is_empty() || parent.is_some_and(|v| v.is_empty() || v == uid || v.contains(['\r','\n'])) { return Err("Invalid task parent".into()); }
    fn is_parent(line: &IcalLine<'_>) -> Result<bool, String> {
        if !line.name.get().eq_ignore_ascii_case("RELATED-TO") { return Ok(false); }
        let params: Vec<_> = line.params.iter().filter(|p| p.name.get().eq_ignore_ascii_case("RELTYPE")).collect();
        if params.len() > 1 || params.first().is_some_and(|p| p.values.len() != 1) { return Err("Ambiguous relation type".into()); }
        Ok(params.first().map(|p| p.values[0].get().trim_matches('"').eq_ignore_ascii_case("PARENT")).unwrap_or(true))
    }
    let mut tree = parse(raw)?; let mut targets = 0;
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if name(child).as_deref() != Some("VTODO") { continue; }
        let ids = values(child,"UID");
        if ids.len() != 1 || ids[0] != uid || !values(child,"RECURRENCE-ID").is_empty() { continue; }
        targets += 1;
        let mut parent_indexes = Vec::new();
        for (i, entry) in child.items.iter().enumerate() {
            if let IcalItem::Prop(line) = entry { if is_parent(line)? { parent_indexes.push(i); } }
        }
        if parent_indexes.len() > 1 { return Err("Ambiguous parent relationship".into()); }
        match (parent_indexes.first(), parent) {
            (Some(index), Some(parent)) => if let IcalItem::Prop(line) = &mut child.items[*index] { line.value.set(&[parent]); },
            (Some(index), None) => { child.items.remove(*index); },
            (None, Some(parent)) => { let mut line = IcalLine::text("RELATED-TO", ""); line.value.set(&[parent]); child.items.push(IcalItem::Prop(line)); },
            (None, None) => {}
        }
    }
    if targets != 1 { return Err("Task target is missing or ambiguous".into()); }
    Ok(tree.to_bytes())
}

pub fn preview_task_parent(raw: &[u8], uid: &str, parent: Option<&str>) -> Result<(String,Option<String>,Vec<u8>), String> {
    let index = task_index(raw)?;
    if index.uid != uid { return Err("Task identity mismatch".into()); }
    if index.parent_uid.as_deref() == parent { return Err("Task already has this parent".into()); }
    let tree = parse(raw)?;
    if !values(&tree,"METHOD").is_empty() || tree.items.iter().any(|item| matches!(item,IcalItem::Opaque(_))) { return Err("Task effect cannot be previewed safely".into()); }
    let mut target = None;
    for item in &tree.items { if let IcalItem::Component(child) = item {
        if name(child).as_deref() == Some("VTIMEZONE") { continue; }
        if name(child).as_deref() != Some("VTODO") || target.replace(child).is_some() { return Err("Task series requires expanded approval".into()); }
    } }
    let target = target.ok_or("Task missing")?;
    if ["ORGANIZER","ATTENDEE","RRULE","RDATE","RECURRENCE-ID","EXDATE","EXRULE"].iter().any(|key| !values(target,key).is_empty()) || target.items.iter().any(|item| matches!(item,IcalItem::Component(_) | IcalItem::Opaque(_))) { return Err("Scheduled, recurring or alarmed task requires expanded approval".into()); }
    let summary = values(target,"SUMMARY");
    if summary.len() != 1 || summary[0].is_empty() || summary[0].len() > 4096 || summary[0].chars().any(char::is_control) { return Err("Task summary cannot be displayed safely".into()); }
    Ok((summary[0].clone(),index.parent_uid,task_parent(raw,uid,parent)?))
}

pub fn preview_task_delete(raw: &[u8], uid: &str) -> Result<(String,Option<String>), String> {
    let index = task_index(raw)?;
    if index.uid != uid { return Err("Task identity mismatch".into()); }
    let tree = parse(raw)?;
    if !values(&tree,"METHOD").is_empty() || tree.items.iter().any(|item| matches!(item,IcalItem::Opaque(_))) { return Err("Task effect cannot be previewed safely".into()); }
    let mut target = None;
    for item in &tree.items { if let IcalItem::Component(child) = item {
        if name(child).as_deref() == Some("VTIMEZONE") { continue; }
        if name(child).as_deref() != Some("VTODO") || target.replace(child).is_some() { return Err("Task series requires expanded approval".into()); }
    } }
    let target = target.ok_or("Task missing")?;
    if ["ORGANIZER","ATTENDEE","RRULE","RDATE","RECURRENCE-ID","EXDATE","EXRULE"].iter().any(|key| !values(target,key).is_empty()) || target.items.iter().any(|item| matches!(item,IcalItem::Component(_) | IcalItem::Opaque(_))) { return Err("Scheduled, recurring or alarmed task requires expanded approval".into()); }
    let summary = values(target,"SUMMARY");
    if summary.len() != 1 || summary[0].is_empty() || summary[0].len() > 4096 || summary[0].chars().any(char::is_control) { return Err("Task summary cannot be displayed safely".into()); }
    Ok((summary[0].clone(),index.parent_uid))
}

/// Change the series recurrence rule on the unique master. Occurrence-specific
/// recurrence rule changes are invalid; overrides and unknown data are kept.
pub fn series_recurrence(raw: &[u8], uid: &str, rule: Option<&str>) -> Result<Vec<u8>, String> {
    if uid.is_empty() { return Err("Missing series UID".into()); }
    if let Some(rule) = rule {
        if rule.is_empty() || rule.len() > 4096 || rule.contains(['\r','\n']) { return Err("Invalid recurrence rule".into()); }
        ical::recur::IcalRecurRule::parse(rule).map_err(|_| "Invalid recurrence rule")?.validate().map_err(|_| "Invalid recurrence rule")?;
    }
    let mut tree = parse(raw)?; let mut matches = 0;
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if !matches!(name(child).as_deref(),Some("VEVENT" | "VTODO")) { continue; }
        let ids = values(child,"UID");
        if ids.len() != 1 || ids[0] != uid || !values(child,"RECURRENCE-ID").is_empty() { continue; }
        matches += 1;
        if values(child,"RRULE").len() > 1 { return Err("Ambiguous recurrence rule".into()); }
        child.items.retain(|entry| !matches!(entry, IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case("RRULE")));
        if let Some(rule) = rule { child.items.push(IcalItem::Prop(IcalLine::text("RRULE", rule.to_owned()))); }
    }
    if matches != 1 { return Err("Series master is missing or ambiguous".into()); }
    Ok(tree.to_bytes())
}

pub struct SeriesRulePreview {
    pub summary: String,
    pub start: String,
    pub end: String,
    pub old_rule: String,
    pub new_rule: String,
    pub old_occurrences: Vec<String>,
    pub new_occurrences: Vec<String>,
    pub raw: Vec<u8>,
}

/// Preview the complete old and new occurrence sets for a bounded UTC series.
/// Exceptions, additional recurrence sets, scheduling and alarms require a
/// broader preview and therefore are not accepted by this operation.
pub fn preview_series_rule(raw: &[u8], uid: &str, rule: &str) -> Result<SeriesRulePreview, String> {
    let tree = parse(raw)?;
    if !values(&tree,"METHOD").is_empty() || tree.items.iter().any(|item| matches!(item,IcalItem::Opaque(_))) { return Err("Series effect cannot be previewed safely".into()); }
    let mut target = None;
    for item in &tree.items { if let IcalItem::Component(child) = item {
        if name(child).as_deref() == Some("VTIMEZONE") { continue; }
        if name(child).as_deref() != Some("VEVENT") || target.replace(child).is_some() { return Err("Series exceptions require an expanded preview".into()); }
    } }
    let target = target.ok_or("Series missing")?;
    let ids = values(target,"UID");
    if ids.len() != 1 || ids[0] != uid { return Err("Series identity mismatch".into()); }
    if ["ORGANIZER","ATTENDEE","RECURRENCE-ID","RDATE","EXDATE","EXRULE","DURATION"].iter().any(|key| !values(target,key).is_empty()) || target.items.iter().any(|item| matches!(item,IcalItem::Component(_) | IcalItem::Opaque(_))) { return Err("Complex or scheduled series needs expanded approval".into()); }
    for item in &target.items { if let IcalItem::Prop(line) = item {
        if ["DTSTART","DTEND","RRULE"].iter().any(|key| line.name.get().eq_ignore_ascii_case(key)) && !line.params.is_empty() { return Err("Parameterized series needs zone-aware preview".into()); }
    } }
    let one = |key: &str| -> Result<String,String> {
        let found = values(target,key);
        if found.len() != 1 || found[0].is_empty() || found[0].len() > 4096 || found[0].chars().any(char::is_control) { return Err("Series property is missing or ambiguous".into()); }
        Ok(found[0].clone())
    };
    let summary = one("SUMMARY")?; let start = one("DTSTART")?; let end = one("DTEND")?;
    let old_rules = values(target,"RRULE");
    if old_rules.len() > 1 || old_rules.first().is_some_and(|value| value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control)) { return Err("Series rule is ambiguous".into()); }
    let old_rule = old_rules.into_iter().next().unwrap_or_default();
    let utc = |value: &str| value.len() == 16 && value.as_bytes()[8] == b'T' && value.ends_with('Z') && ical::recur::IcalRecurDateTime::parse(value).is_ok();
    if !utc(&start) || !utc(&end) || start >= end { return Err("Series times must be UTC".into()); }
    let expand = |rule: &str| -> Result<Vec<String>,String> {
        if rule.contains(['\r','\n']) { return Err("Invalid recurrence rule".into()); }
        let set: icalendar::RRuleSet = format!("DTSTART:{start}\nRRULE:{rule}").parse().map_err(|_| "Invalid recurrence rule")?;
        if set.get_rrule().len() != 1 || !set.get_rrule()[0].get_count().is_some_and(|count| (1..=100).contains(&count)) { return Err("Series must have a finite count of at most 100".into()); }
        let result = set.all(101);
        if result.limited || result.dates.is_empty() { return Err("Series expansion is incomplete".into()); }
        let dates: Vec<String> = result.dates.iter().map(|date| date.format("%Y%m%dT%H%M%SZ").to_string()).collect();
        if dates.first() != Some(&start) { return Err("Start does not match the recurrence rule".into()); }
        Ok(dates)
    };
    let old_occurrences = if old_rule.is_empty() {vec![start.clone()]} else {expand(&old_rule)?}; let new_occurrences = expand(rule)?;
    Ok(SeriesRulePreview {summary,start,end,old_rule,new_rule:rule.into(),old_occurrences,new_occurrences,raw:series_recurrence(raw,uid,Some(rule))?})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_time_edits_only_selected_occurrence_and_rejects_unsafe_conversions() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTART:20261101T100000Z\r\nDTEND:20261101T110000Z\r\nRRULE:FREQ=DAILY\r\nX-KEEP:original\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:event\r\nRECURRENCE-ID:20261102T100000Z\r\nDTSTART:20261102T100000Z\r\nDTEND:20261102T110000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let edited = event_time(raw, "event", Some("20261102T100000Z"), "20261102T120000Z", "20261102T130000Z", false).unwrap();
        let text = String::from_utf8(edited).unwrap();
        assert!(text.contains("DTSTART:20261101T100000Z\r\nDTEND:20261101T110000Z\r\nRRULE:FREQ=DAILY\r\nX-KEEP:original"));
        assert!(text.contains("DTSTART:20261102T120000Z"));
        assert!(event_time(raw, "event", None, "20261102", "20261102", true).is_err());
        let zoned = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTART;TZID=Europe/Vienna:20261101T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(event_time(zoned, "event", None, "20261101T100000Z", "20261101T110000Z", false).is_err());
    }

    #[test]
    fn zone_edit_rejects_dst_gap_and_fold_and_preserves_other_data() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261101T100000Z\r\nDTSTART:20261101T100000Z\r\nDTEND:20261101T110000Z\r\nX-KEEP:yes\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let edited = event_time_zoned(raw,"event",None,"20261102T100000","20261102T110000","Europe/Vienna").unwrap();
        let text = String::from_utf8(edited).unwrap();
        assert!(text.contains("DTSTART;TZID=Europe/Vienna:20261102T100000"));
        assert!(text.contains("X-KEEP:yes"));
        assert!(event_time_zoned(raw,"event",None,"20260329T023000","20260329T033000","Europe/Vienna").is_err());
        assert!(event_time_zoned(raw,"event",None,"20261025T023000","20261025T033000","Europe/Vienna").is_err());
    }

    #[test]
    fn invitation_preview_binds_organizer_previous_response_and_target() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nORGANIZER:mailto:organizer@example.test\r\nATTENDEE;PARTSTAT=TENTATIVE:mailto:person@example.test\r\nX-KEEP:yes\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let (organizer, old, edited) = preview_attendee_response(raw,"event",None,"mailto:person@example.test","accept").unwrap();
        assert_eq!(organizer,"mailto:organizer@example.test");
        assert_eq!(old,"TENTATIVE");
        let text = String::from_utf8(edited).unwrap();
        assert!(text.contains("PARTSTAT=ACCEPTED"));
        assert!(text.contains("X-KEEP:yes"));
        assert!(preview_attendee_response(raw,"event",None,"mailto:other@example.test","accept").is_err());
    }

    #[test]
    fn exception_requires_server_occurrence_and_keeps_master_properties() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTART:20261101T100000Z\r\nDTEND:20261101T110000Z\r\nRRULE:FREQ=DAILY\r\nX-KEEP:yes\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let expanded = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nRECURRENCE-ID:20261102T100000Z\r\nDTSTART:20261102T100000Z\r\nDTEND:20261102T110000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let edited = create_event_exception(raw,expanded,"event","20261102T100000Z","20261102T120000Z","20261102T130000Z").unwrap();
        let text = String::from_utf8(edited.clone()).unwrap();
        assert_eq!(text.matches("X-KEEP:yes").count(),2);
        assert_eq!(text.matches("RRULE:FREQ=DAILY").count(),1);
        assert!(text.contains("RECURRENCE-ID:20261102T100000Z"));
        assert!(create_event_exception(&edited,expanded,"event","20261102T100000Z","20261102T120000Z","20261102T130000Z").is_err());
        assert!(create_event_exception(raw,expanded,"event","20261103T100000Z","20261103T120000Z","20261103T130000Z").is_err());
    }

    #[test]
    fn simple_delete_preview_refuses_scheduling_and_recurrence() {
        let simple = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nSUMMARY:Planning\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(preview_event_delete(simple).unwrap(),("event".into(),"Planning".into()));
        let recurring = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nSUMMARY:Planning\r\nRRULE:FREQ=DAILY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(preview_event_delete(recurring).is_err());
        let invited = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nSUMMARY:Planning\r\nATTENDEE:mailto:a@example.test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(preview_event_delete(invited).is_err());
        let attached = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nSUMMARY:Planning\r\nATTACH;ENCODING=BASE64;VALUE=BINARY:QUJD\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(preview_event_delete(attached).is_err());
        let contact = b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:contact\r\nFN:Example\r\nEND:VCARD\r\n";
        assert_eq!(preview_contact_delete(contact).unwrap(),("contact".into(),"Example".into()));
    }

    #[test]
    fn typed_create_escapes_text_and_requires_valid_event_range() {
        let event = create_simple("event","1234","20261101T100000Z","Review, design; costs",Some("20261102T100000Z"),Some("20261102T110000Z")).unwrap();
        let text = String::from_utf8(event.clone()).unwrap();
        assert!(text.contains("SUMMARY:Review\\, design\\; costs"));
        assert!(parse(&event).is_ok());
        assert!(create_simple("event","1234","20261101T100000Z","Review",Some("20261102T110000Z"),Some("20261102T100000Z")).is_err());
        let contact = create_simple("contact","1234","20261101T100000Z","Müller",None,None).unwrap();
        assert!(preview_contact_delete(&contact).is_ok());
    }

    #[test]
    fn contact_field_preview_keeps_version_group_parameters_and_org_units() {
        let raw = b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:id\r\nFN:Example\r\nitem1.TEL;VALUE=uri;TYPE=work:tel:+431234\r\nORG:Old company;Research\r\nPHOTO;VALUE=uri:https://example.test/photo\r\nEND:VCARD\r\n";
        let phone = preview_contact_field(raw,"TEL",0,"tel:+431111").unwrap();
        assert_eq!(phone.version,"4.0"); assert_eq!(phone.property_name,"item1.TEL"); assert_eq!(phone.value_type,"uri"); assert_eq!(phone.old_value,"tel:+431234");
        assert!(String::from_utf8(phone.raw).unwrap().contains("item1.TEL;VALUE=uri;TYPE=work:tel:+431111"));
        let org = preview_contact_field(raw,"ORG",0,"New company").unwrap();
        let text = String::from_utf8(org.raw).unwrap();
        assert!(text.contains("ORG:New company;Research"));
        assert!(text.contains("PHOTO;VALUE=uri:https://example.test/photo"));
        assert!(preview_contact_field(raw,"TEL",0,"not-a-uri").is_err());
        let added = preview_contact_add(raw,"EMAIL","new@example.test","text").unwrap();
        let text = String::from_utf8(added.raw).unwrap();
        assert!(text.contains("EMAIL:new@example.test"));
        assert!(text.contains("PHOTO;VALUE=uri:https://example.test/photo"));
        let removed = preview_contact_remove(raw,"ORG",0).unwrap();
        assert!(removed.old_value.contains("Research"));
        assert!(!String::from_utf8(removed.raw).unwrap().contains("ORG:"));
    }

    #[test]
    fn photo_change_preserves_other_contact_fields() {
        let raw = b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:id\r\nFN:Example\r\nPHOTO;VALUE=uri:https://example.test/old\r\nX-KEEP:yes\r\nEND:VCARD\r\n";
        let (old,changed) = contact_photo(raw,"data:image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(old.unwrap(),b"https://example.test/old");
        let text = String::from_utf8(changed).unwrap();
        assert!(text.contains("PHOTO;VALUE=uri:data:image/png;base64,iVBORw0KGgo="));
        assert!(text.contains("X-KEEP:yes"));
        assert!(contact_photo(raw,"https://example.test/not-embedded").is_err());
    }

    #[test]
    fn task_attribute_preview_excludes_series_and_scheduling() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:task\r\nSUMMARY:Prepare\r\nPRIORITY:5\r\nDUE:20261102T100000Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let (name,priority,due,edited) = preview_task_attributes(raw,"task",Some(2),Some(("20261103T100000Z",false))).unwrap();
        assert_eq!(name,"Prepare"); assert_eq!(priority.as_deref(),Some("5")); assert_eq!(due.as_deref(),Some("20261102T100000Z"));
        assert!(String::from_utf8(edited).unwrap().contains("PRIORITY:2"));
        let recurring = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:task\r\nSUMMARY:Prepare\r\nRRULE:FREQ=DAILY\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert!(preview_task_attributes(recurring,"task",Some(2),None).is_err());
    }

    #[test]
    fn cancellation_preview_binds_recipients_and_sequence() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nSUMMARY:Planning\r\nORGANIZER:mailto:organizer@example.test\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:person@example.test\r\nSEQUENCE:3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let preview = preview_event_cancellation(raw,"event",None,4).unwrap();
        assert_eq!(preview.organizer,"mailto:organizer@example.test");
        assert_eq!(preview.attendees,vec!["mailto:person@example.test"]);
        assert_eq!((preview.old_sequence,preview.new_sequence),(3,4));
        assert!(String::from_utf8(preview.raw).unwrap().contains("STATUS:CANCELLED"));
        assert!(preview_event_cancellation(raw,"event",None,3).is_err());
    }

    #[test]
    fn invitation_create_has_exact_explicit_participants() {
        let attendees = vec!["mailto:a@example.test".to_owned(),"mailto:b@example.test".to_owned()];
        let raw = create_invitation("id","20261101T100000Z","Meeting","20261102T100000Z","20261102T110000Z","mailto:owner@example.test",&attendees).unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("ORGANIZER:mailto:owner@example.test"));
        assert!(text.contains("ATTENDEE;RSVP=TRUE:mailto:a@example.test"));
        assert!(text.contains("ATTENDEE;RSVP=TRUE:mailto:b@example.test"));
        assert!(create_invitation("id","20261101T100000Z","Meeting","20261102T100000Z","20261102T110000Z","mailto:owner@example.test",&["mailto:@".to_owned()]).is_err());
    }

    #[test]
    fn series_rule_preview_shows_complete_bounded_occurrence_sets() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nSUMMARY:Planning\r\nDTSTART:20261101T100000Z\r\nDTEND:20261101T110000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let preview = preview_series_rule(raw,"event","FREQ=WEEKLY;COUNT=2").unwrap();
        assert_eq!(preview.old_occurrences,vec!["20261101T100000Z","20261102T100000Z","20261103T100000Z"]);
        assert_eq!(preview.new_occurrences,vec!["20261101T100000Z","20261108T100000Z"]);
        assert!(preview_series_rule(raw,"event","FREQ=DAILY").is_err());
        let single = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nSUMMARY:Planning\r\nDTSTART:20261101T100000Z\r\nDTEND:20261101T110000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let created = preview_series_rule(single,"event","FREQ=DAILY;COUNT=2").unwrap();
        assert!(created.old_rule.is_empty()); assert_eq!(created.old_occurrences,vec!["20261101T100000Z"]);
    }

    #[test]
    fn parent_preview_shows_old_and_new_relationship_without_touching_other_fields() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:child\r\nSUMMARY:Work\r\nRELATED-TO:old\r\nX-KEEP:yes\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let (summary,old,changed) = preview_task_parent(raw,"child",Some("new")).unwrap();
        assert_eq!(summary,"Work"); assert_eq!(old.as_deref(),Some("old"));
        let text = String::from_utf8(changed).unwrap();
        assert!(text.contains("RELATED-TO:new")); assert!(text.contains("X-KEEP:yes"));
        assert!(preview_task_parent(raw,"child",Some("old")).is_err());
    }

    #[test]
    fn contact_email_edit_preserves_group_type_photo_and_other_addresses() {
        let raw = b"BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Example\r\nitem1.EMAIL;TYPE=work:old@example.test\r\nEMAIL;TYPE=home:home@example.test\r\nPHOTO;VALUE=uri:https://example.test/photo\r\nX-KEEP:yes\r\nEND:VCARD\r\n";
        let edited = edit_contact_text(raw, "EMAIL", 0, "new@example.test").unwrap();
        let text = String::from_utf8(edited).unwrap();
        assert!(text.contains("item1.EMAIL;TYPE=work:new@example.test"));
        assert!(text.contains("EMAIL;TYPE=home:home@example.test"));
        assert!(text.contains("PHOTO;VALUE=uri:https://example.test/photo"));
        assert!(text.contains("X-KEEP:yes"));
    }

    #[test]
    fn text_preview_preserves_original_and_rejects_scheduling() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nSUMMARY:Before\r\nX-KEEP:yes\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let (old, edited) = preview_text_edit(raw,"event",None,"SUMMARY","After").unwrap();
        assert_eq!(old.as_deref(),Some("Before"));
        let edited = String::from_utf8(edited).unwrap();
        assert!(edited.contains("SUMMARY:After\r\n"));
        assert!(edited.contains("X-KEEP:yes\r\n"));
        assert!(raw.windows(14).any(|w| w == b"SUMMARY:Before"));
        let scheduled = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nSUMMARY:Before\r\nATTENDEE:mailto:a@example.test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(preview_text_edit(scheduled,"event",None,"SUMMARY","After").is_err());
    }

    #[test]
    fn recovered_contact_changes_uid_and_preserves_photo() {
        let raw = b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:original\r\nFN:Example\r\nPHOTO:data:image/png;base64,AQID\r\nEND:VCARD\r\n";
        let output = String::from_utf8(recovered_contact(raw,"recovered").unwrap()).unwrap();
        assert!(output.contains("UID:recovered\r\n"));
        assert!(output.contains("PHOTO:data:image/png;base64,AQID\r\n"));
        assert!(recovered_contact(raw,"original").is_err());
    }

    const SOURCE: &[u8] = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Vienna\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:one\r\nDTSTART;TZID=Europe/Vienna:20261025T100000\r\nSUMMARY:Old\r\nX-KEEP;X-PARAM=YES:unchanged\r\nORGANIZER:mailto:owner@example.test\r\nATTENDEE:mailto:guest@example.test\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT5M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn text_edit_keeps_unknown_timezone_and_other_original_lines() {
        let edited = edit_text(SOURCE,"one",None,&[("SUMMARY","New, value")]).unwrap();
        let text = String::from_utf8(edited).unwrap();
        assert!(text.contains("X-KEEP;X-PARAM=YES:unchanged\r\n"));
        assert!(text.contains("DTSTART;TZID=Europe/Vienna:20261025T100000\r\n"));
        assert!(text.contains("SUMMARY:New\\, value\r\n"));
        assert!(text.contains("BEGIN:VALARM\r\n"));
    }

    #[test]
    fn recovered_copy_strips_scheduling_and_alarm_and_keeps_original() {
        let map = HashMap::from([("one".to_string(),"replacement".to_string())]);
        let output = recovered_calendar(SOURCE,&map).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("UID:replacement\r\n"));
        assert!(text.contains("X-KEEP;X-PARAM=YES:unchanged\r\n"));
        assert!(text.contains("BEGIN:VTIMEZONE\r\n"));
        for forbidden in ["METHOD:","ORGANIZER:","ATTENDEE:","BEGIN:VALARM"] { assert!(!text.contains(forbidden)); }
        assert!(SOURCE.windows(11).any(|w| w == b"METHOD:REQU"));
    }

    #[test]
    fn recovery_maps_parentage_and_rejects_external_parent() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:parent\r\nEND:VTODO\r\nBEGIN:VTODO\r\nUID:child\r\nRELATED-TO;RELTYPE=PARENT:parent\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let map = HashMap::from([("parent".into(),"new-parent".into()),("child".into(),"new-child".into())]);
        let output = String::from_utf8(recovered_calendar(raw,&map).unwrap()).unwrap();
        assert!(output.contains("RELATED-TO;RELTYPE=PARENT:new-parent\r\n"));
        assert!(recovered_calendar(raw,&HashMap::from([("child".into(),"new-child".into())])).is_err());
        let child = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:child\r\nRELATED-TO;RELTYPE=PARENT:parent\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let output = String::from_utf8(recovered_calendar(child,&map).unwrap()).unwrap();
        assert!(output.contains("RELATED-TO;RELTYPE=PARENT:new-parent\r\n"));
    }

    #[test]
    fn edit_targets_one_occurrence_and_rejects_ambiguous_master() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:series\r\nSUMMARY:Master\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:series\r\nRECURRENCE-ID:20261028T100000Z\r\nSUMMARY:Exception\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let output = String::from_utf8(edit_text(raw,"series",Some("20261028T100000Z"),&[("SUMMARY","Changed")]).unwrap()).unwrap();
        assert!(output.contains("SUMMARY:Master\r\n"));
        assert!(output.contains("SUMMARY:Changed\r\n"));
        assert!(edit_text(raw,"series",Some("missing"),&[("SUMMARY","Changed")]).is_err());
    }

    #[test]
    fn contact_edit_preserves_unknown_fields_and_photo() {
        let raw = b"BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Old\r\nPHOTO:data:image/png;base64,QUJD\r\nX-KEEP;TYPE=work:untouched\r\nEND:VCARD\r\n";
        let result = String::from_utf8(edit_contact_text(raw,"FN",0,"New, Name").unwrap()).unwrap();
        assert!(result.contains("FN:New\\, Name\r\n"));
        assert!(result.contains("PHOTO:data:image/png;base64,QUJD\r\n"));
        assert!(result.contains("X-KEEP;TYPE=work:untouched\r\n"));
        assert!(edit_contact_text(raw,"FN",1,"missing").is_err());
    }

    #[test]
    fn contact_name_preview_and_edit_share_one_unambiguous_source() {
        let raw = b"BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Old\\, Name\r\nX-KEEP:untouched\r\nEND:VCARD\r\n";
        let (old, changed) = edit_unique_contact_name(raw,"New Name").unwrap();
        assert_eq!(old,"Old, Name");
        assert!(String::from_utf8(changed).unwrap().contains("X-KEEP:untouched\r\n"));
        let ambiguous = b"BEGIN:VCARD\r\nVERSION:4.0\r\nFN:One\r\nFN:Two\r\nEND:VCARD\r\n";
        assert!(edit_unique_contact_name(ambiguous,"New").is_err());
        let localized = b"BEGIN:VCARD\r\nVERSION:4.0\r\nFN;LANGUAGE=en:One\r\nEND:VCARD\r\n";
        assert!(edit_unique_contact_name(localized,"New").is_err());
    }

    #[test]
    fn task_completion_preview_binds_prior_state_and_excludes_scheduling() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:task\r\nSUMMARY:Example\r\nSTATUS:IN-PROCESS\r\nPERCENT-COMPLETE:25\r\nX-KEEP:untouched\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let (summary,status,completed,percent,changed) = preview_task_completion(raw,"task",None,Some("20261028T120000Z")).unwrap();
        assert_eq!(summary,"Example");
        assert_eq!(status.as_deref(),Some("IN-PROCESS"));
        assert_eq!(completed,None);
        assert_eq!(percent.as_deref(),Some("25"));
        let changed = String::from_utf8(changed).unwrap();
        assert!(changed.contains("COMPLETED:20261028T120000Z\r\n"));
        assert!(changed.contains("X-KEEP:untouched\r\n"));
        let scheduled = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:task\r\nSUMMARY:Example\r\nATTENDEE:mailto:a@example.test\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert!(preview_task_completion(scheduled,"task",None,None).is_err());
        let series = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:task\r\nSUMMARY:Example\r\nRRULE:FREQ=DAILY\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert!(preview_task_completion(series,"task",None,None).is_err());
    }

    #[test]
    fn task_completion_keeps_native_parentage_and_reopens() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:child\r\nRELATED-TO;RELTYPE=PARENT:parent\r\nRRULE:FREQ=WEEKLY\r\nX-KEEP:test\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let completed = task_completion(raw,"child",None,Some("20261028T120000Z")).unwrap();
        let view = String::from_utf8(completed.clone()).unwrap();
        assert!(view.contains("STATUS:COMPLETED\r\n"));
        assert!(view.contains("COMPLETED:20261028T120000Z\r\n"));
        assert!(view.contains("RELATED-TO;RELTYPE=PARENT:parent\r\n"));
        assert!(view.contains("RRULE:FREQ=WEEKLY\r\n"));
        let reopened = String::from_utf8(task_completion(&completed,"child",None,None).unwrap()).unwrap();
        assert!(reopened.contains("STATUS:NEEDS-ACTION\r\n"));
        assert!(!reopened.contains("COMPLETED:"));
    }

    #[test]
    fn rsvp_changes_only_selected_attendee_and_preserves_parameters() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nATTENDEE;CN=Person;PARTSTAT=NEEDS-ACTION:mailto:person@example.test\r\nATTENDEE;CN=Other:mailto:other@example.test\r\nX-KEEP:original\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let output = String::from_utf8(respond_attendee(raw,"event",None,"mailto:person@example.test","accept").unwrap()).unwrap();
        assert!(output.contains("ATTENDEE;CN=Person;PARTSTAT=ACCEPTED:mailto:person@example.test\r\n"));
        assert!(output.contains("ATTENDEE;CN=Other:mailto:other@example.test\r\n"));
        assert!(output.contains("X-KEEP:original\r\n"));
        assert!(respond_attendee(raw,"event",None,"mailto:missing@example.test","accept").is_err());
    }

    #[test]
    fn cancel_event_requires_advanced_sequence_and_keeps_attendees() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event\r\nSEQUENCE:4\r\nATTENDEE:mailto:person@example.test\r\nX-KEEP:yes\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(cancel_event(raw,"event",None,4).is_err());
        let output = String::from_utf8(cancel_event(raw,"event",None,5).unwrap()).unwrap();
        assert!(output.contains("STATUS:CANCELLED\r\n"));
        assert!(output.contains("SEQUENCE:5\r\n"));
        assert!(output.contains("ATTENDEE:mailto:person@example.test\r\n"));
        assert!(output.contains("X-KEEP:yes\r\n"));
    }

    #[test]
    fn task_attributes_keep_parentage_and_explicit_date_type() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:child\r\nRELATED-TO:parent\r\nX-KEEP:yes\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let output = String::from_utf8(task_attributes(raw,"child",None,Some(3),Some(("20261110",true))).unwrap()).unwrap();
        assert!(output.contains("PRIORITY:3\r\n"));
        assert!(output.contains("DUE;VALUE=DATE:20261110\r\n"));
        assert!(output.contains("RELATED-TO:parent\r\n"));
        assert!(output.contains("X-KEEP:yes\r\n"));
        assert!(task_attributes(raw,"child",None,Some(10),None).is_err());
    }

    #[test]
    fn task_parent_changes_parent_without_touching_siblings() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:child\r\nRELATED-TO;RELTYPE=PARENT;X-KEEP=YES:old\r\nRELATED-TO;RELTYPE=SIBLING:other\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let output = String::from_utf8(task_parent(raw,"child",Some("new")).unwrap()).unwrap();
        assert!(output.contains("RELATED-TO;RELTYPE=PARENT;X-KEEP=YES:new\r\n"));
        assert!(output.contains("RELATED-TO;RELTYPE=SIBLING:other\r\n"));
        assert!(task_parent(raw,"child",Some("child")).is_err());
        assert_eq!(task_index(raw).unwrap(), TaskIndex { uid:"child".into(), parent_uid:Some("old".into()) });
    }

    #[test]
    fn recurrence_edit_preserves_override_and_rejects_invalid_rule() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:series\r\nRRULE:FREQ=DAILY\r\nX-KEEP:yes\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:series\r\nRECURRENCE-ID:20261028T100000Z\r\nSUMMARY:Exception\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(series_recurrence(raw,"series",Some("FREQ=NOPE")).is_err());
        let output = String::from_utf8(series_recurrence(raw,"series",Some("FREQ=WEEKLY;BYDAY=MO,WE")).unwrap()).unwrap();
        assert!(output.contains("RRULE:FREQ=WEEKLY;BYDAY=MO,WE\r\n"));
        assert!(output.contains("SUMMARY:Exception\r\n"));
        assert!(output.contains("X-KEEP:yes\r\n"));
    }
}
