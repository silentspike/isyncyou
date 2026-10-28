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

/// Create a personal recovery projection. Mapping must be generated and
/// persisted before any dispatch; exceptions of a series share one replacement
/// UID. Unmapped parents fail closed rather than linking to an old resource.
pub fn recovered_calendar(raw: &[u8], uid_map: &HashMap<String, String>) -> Result<Vec<u8>, String> {
    if uid_map.is_empty() || uid_map.values().any(|s| s.is_empty() || s.contains(['\r','\n']) || uid_map.contains_key(s)) || uid_map.values().collect::<HashSet<_>>().len() != uid_map.len() { return Err("Invalid recovery UID mapping".into()); }
    let mut tree = parse(raw)?;
    tree.items.retain(|item| !matches!(item, IcalItem::Prop(line) if line.name.get().eq_ignore_ascii_case("METHOD")));
    let mut found = HashSet::new();
    for item in &mut tree.items {
        let IcalItem::Component(child) = item else { continue; };
        if !matches!(name(child).as_deref(), Some("VEVENT" | "VTODO")) { continue; }
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
    if found.len() != uid_map.len() { return Err("Recovery UID mapping does not match source".into()); }
    Ok(tree.to_bytes())
}

/// Edit one selected contact text property while retaining every other wire
/// line, including photos and vendor extensions. `index` disambiguates repeated
/// properties. Non-UTF-8/transfer-encoded target fields are rejected rather than
/// rewriting them with an incorrect charset.
pub fn edit_contact_text(raw: &[u8], key: &str, index: usize, value: &str) -> Result<Vec<u8>, String> {
    if raw.len() > 16 * 1024 * 1024 || !matches!(key, "FN" | "NOTE" | "TITLE" | "ROLE") || value.len() > 64 * 1024 || value.contains('\0') { return Err("Unsupported or invalid contact edit".into()); }
    let mut card = vcard::tree::cst::VcardCst::parse(raw).map_err(|_| "Invalid contact resource")?;
    if card.begin.is_none() || card.to_bytes() != raw { return Err("Contact must be a single complete vCard".into()); }
    let target = card.props.iter_mut().filter(|line| line.name.get().eq_ignore_ascii_case(key)).nth(index).ok_or("Contact property is missing")?;
    if target.params.iter().any(|param| param.name.get().eq_ignore_ascii_case("CHARSET") || param.name.get().eq_ignore_ascii_case("ENCODING")) { return Err("Encoded contact field cannot be safely edited".into()); }
    target.value.set(&[value]);
    Ok(card.to_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;

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
