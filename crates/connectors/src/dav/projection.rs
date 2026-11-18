//! Read-only typed projections over original DAV resources.
//!
//! These projections never replace the stored source bytes. An edit must use a
//! native-preserving editor and a conditional write, not serialize this view.

use ical::tree::cst::{IcalCst, IcalItem};
use serde_json::{json, Map, Value};

fn cst_component(item: &IcalCst<'_>, depth: usize) -> Result<Value, String> {
    if depth > 16 {
        return Err("Calendar nesting exceeds limit".into());
    }
    let kind = item
        .begin
        .as_ref()
        .ok_or("Calendar component has no beginning")?
        .raw_value_str();
    if item
        .end
        .as_ref()
        .map(|line| line.raw_value_str())
        .as_deref()
        != Some(kind.as_ref())
    {
        return Err("Calendar component is incomplete".into());
    }
    let mut properties = Map::new();
    let mut children = Vec::new();
    for entry in &item.items {
        match entry {
            IcalItem::Prop(line) => {
                let name = line.name.get();
                let params: Map<String, Value> = line
                    .params
                    .iter()
                    .map(|p| {
                        (
                            p.name.get().to_owned(),
                            Value::String(
                                p.values
                                    .iter()
                                    .map(|v| {
                                        v.get()
                                            .strip_prefix('"')
                                            .and_then(|v| v.strip_suffix('"'))
                                            .unwrap_or(v.get())
                                    })
                                    .collect::<Vec<_>>()
                                    .join(","),
                            ),
                        )
                    })
                    .collect();
                let value = json!({"value":line.value.decode(), "parameters":params});
                let multiple = matches!(
                    name,
                    "ATTACH"
                        | "ATTENDEE"
                        | "CATEGORIES"
                        | "COMMENT"
                        | "CONTACT"
                        | "EXDATE"
                        | "FREEBUSY"
                        | "IANA-PROP"
                        | "RDATE"
                        | "RELATED"
                        | "RESOURCES"
                        | "RSTATUS"
                        | "X-PROP"
                );
                if multiple {
                    let list = properties
                        .entry(name.to_owned())
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Value::Array(list) = list {
                        list.push(value);
                    }
                } else {
                    properties.insert(name.to_owned(), Value::Array(vec![value]));
                }
            }
            IcalItem::Component(child) => children.push(cst_component(child, depth + 1)?),
            IcalItem::Opaque(_) => return Err("Invalid calendar component".into()),
        }
    }
    let uid = properties
        .get("UID")
        .and_then(|p| p.get(0))
        .and_then(|p| p.get("value"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({"kind":kind, "uid":uid, "properties":properties, "components":children}))
}

/// Project VEVENT/VTODO components including recurrence IDs, rules, attendee
/// parameters, RELATED-TO, dates, and nested alarms as structured fields.
/// This view is intentionally not a recurrence expansion or a source serializer.
pub fn calendar(raw: &[u8]) -> Result<Value, String> {
    if raw.len() > 16 * 1024 * 1024 {
        return Err("Calendar resource is too large".into());
    }
    let tree = ical::tree::cst::IcalCst::parse(raw).map_err(|_| "Invalid calendar resource")?;
    if tree.to_bytes() != raw
        || tree
            .begin
            .as_ref()
            .map(|line| line.raw_value_str())
            .as_deref()
            != Some("VCALENDAR")
        || tree
            .end
            .as_ref()
            .map(|line| line.raw_value_str())
            .as_deref()
            != Some("VCALENDAR")
        || tree
            .items
            .iter()
            .any(|item| matches!(item, IcalItem::Opaque(_)))
    {
        return Err("Calendar must be a single complete resource".into());
    }
    let jcal = tree.decode().to_jcal();
    std::str::from_utf8(raw).map_err(|_| "Calendar resource is not UTF-8")?;
    let mut components = Vec::new();
    for item in &tree.items {
        match item {
            IcalItem::Component(child)
                if matches!(
                    child.begin.as_ref().map(|b| b.raw_value_str()).as_deref(),
                    Some("VEVENT" | "VTODO")
                ) =>
            {
                components.push(cst_component(child, 0)?)
            }
            _ => {}
        }
    }
    Ok(json!({"components":components,"jcal":jcal}))
}

/// Project a complete contact into RFC 7095 jCard while retaining original VCF
/// separately in the provider store, including any unrecognized bytes/fields.
pub fn contact(raw: &[u8]) -> Result<Value, String> {
    if raw.len() > 16 * 1024 * 1024 {
        return Err("Contact resource is too large".into());
    }
    let parsed = vcard::tree::cst::VcardCst::parse(raw).map_err(|_| "Invalid contact resource")?;
    if parsed.to_bytes() != raw {
        return Err("Contact resource is not a single complete vCard".into());
    }
    Ok(parsed.decode().to_jcard())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cst_projection_matches_existing_calendar_projection() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nSUMMARY:First\r\nSUMMARY:Second\\, folded\r\n line\r\nATTENDEE;CN=Person;ROLE=REQ-PARTICIPANT:mailto:a@example.test\r\nATTENDEE;CN=Other:mailto:b@example.test\r\nEXDATE:20261102T100000Z\r\nEXDATE:20261103T100000Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VTODO\r\nUID:task\r\nRELATED-TO:parent\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let view = calendar(raw).unwrap();
        assert_eq!(
            view["components"][0]["properties"]["SUMMARY"][0]["value"],
            "Second, foldedline"
        );
        assert_eq!(
            view["components"][0]["properties"]["ATTENDEE"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            view["components"][0]["properties"]["EXDATE"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(view["components"][0]["components"][0]["kind"], "VALARM");
        assert_eq!(view["components"][1]["uid"], "task");
    }

    #[test]
    fn cst_projection_matches_parameter_and_list_values() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTART;TZID=Europe/Vienna:20261101T100000\r\nCATEGORIES:ONE,TWO\r\nATTENDEE;CN=\"Person, Example\":mailto:x@example.test\r\nATTACH:https://example.test/a;b,c\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let view = calendar(raw).unwrap();
        let properties = &view["components"][0]["properties"];
        assert_eq!(
            properties["ATTENDEE"][0]["parameters"]["CN"],
            "Person, Example"
        );
        assert_eq!(
            properties["DTSTART"][0]["parameters"]["TZID"],
            "Europe/Vienna"
        );
        assert_eq!(properties["CATEGORIES"][0]["value"], "ONE,TWO");
        assert_eq!(
            properties["ATTACH"][0]["value"],
            "https://example.test/a;b,c"
        );
    }

    #[test]
    fn calendar_projection_rejects_incomplete_components() {
        assert!(
            calendar(b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:event\r\nEND:VCALENDAR\r\n").is_err()
        );
        assert!(
            calendar(b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:event\r\nEND:VEVENT\r\n").is_err()
        );
    }

    #[test]
    fn calendar_view_keeps_native_task_parentage_and_recurrence() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VTODO\r\nUID:child\r\nRELATED-TO:parent\r\nSUMMARY:Work\r\nRRULE:FREQ=WEEKLY\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let view = calendar(raw).unwrap();
        assert_eq!(view["components"][0]["kind"], "VTODO");
        assert_eq!(
            view["components"][0]["properties"]["RELATED-TO"][0]["value"],
            "parent"
        );
        assert_eq!(
            view["components"][0]["properties"]["RRULE"][0]["value"],
            "FREQ=WEEKLY"
        );
        assert_eq!(view["jcal"][0], "vcalendar");
        assert!(!view.to_string().contains("checklist"));
    }

    #[test]
    fn vcard_view_is_jcard_and_source_remains_unchanged() {
        let raw =
            b"BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Example\r\nX-CUSTOM;TYPE=work:kept\r\nEND:VCARD\r\n";
        let view = contact(raw).unwrap();
        assert_eq!(view[0], "vcard");
        assert!(view.to_string().contains("Example"));
        assert!(view.to_string().contains("x-custom"));
        assert!(raw.windows(8).any(|w| w == b"X-CUSTOM"));
    }

    #[test]
    fn calendar_view_retains_series_exception_and_native_task_fields() {
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261001T100000Z\r\nDTSTART:20261028T100000Z\r\nSUMMARY:Series\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEXDATE:20261029T100000Z\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTAMP:20261001T100000Z\r\nRECURRENCE-ID:20261030T100000Z\r\nDTSTART:20261030T110000Z\r\nSUMMARY:Changed\r\nEND:VEVENT\r\nBEGIN:VTODO\r\nUID:task\r\nDTSTAMP:20261001T100000Z\r\nSUMMARY:Task\r\nDUE:20261029T100000Z\r\nSTATUS:NEEDS-ACTION\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let view = calendar(raw).unwrap();
        let components = view["components"].as_array().unwrap();
        assert_eq!(components.len(), 3);
        assert_eq!(
            components[0]["properties"]["RRULE"][0]["value"],
            "FREQ=DAILY;COUNT=3"
        );
        assert_eq!(
            components[0]["properties"]["EXDATE"][0]["value"],
            "20261029T100000Z"
        );
        assert_eq!(
            components[1]["properties"]["RECURRENCE-ID"][0]["value"],
            "20261030T100000Z"
        );
        assert_eq!(components[2]["kind"], "VTODO");
        assert_eq!(
            components[2]["properties"]["DUE"][0]["value"],
            "20261029T100000Z"
        );
        assert_eq!(
            components[2]["properties"]["STATUS"][0]["value"],
            "NEEDS-ACTION"
        );
        assert_eq!(view["jcal"][0], "vcalendar");
    }
}
