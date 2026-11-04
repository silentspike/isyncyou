//! Read-only typed projections over original DAV resources.
//!
//! These projections never replace the stored source bytes. An edit must use a
//! native-preserving editor and a conditional write, not serialize this view.

use icalendar::{Calendar, CalendarComponent, Component, Property};
use serde_json::{json, Map, Value};

fn property(value: &Property) -> Value {
    let params: Map<String, Value> = value
        .params()
        .iter()
        .map(|(name, param)| (name.clone(), Value::String(param.value().to_owned())))
        .collect();
    json!({"value":value.value(),"parameters":params})
}

fn component<T: Component>(item: &T, depth: usize) -> Result<Value, String> {
    if depth > 16 {
        return Err("Calendar nesting exceeds limit".into());
    }
    let mut properties = Map::new();
    for (name, value) in item.properties() {
        properties.insert(name.clone(), Value::Array(vec![property(value)]));
    }
    for (name, values) in item.multi_properties() {
        let list = properties
            .entry(name.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(list) = list {
            list.extend(values.iter().map(property));
        }
    }
    let children: Vec<Value> = item
        .components()
        .iter()
        .map(|child| component(child, depth + 1))
        .collect::<Result<_, _>>()?;
    Ok(
        json!({"kind":item.component_kind(), "uid":item.get_uid(), "properties":properties,"components":children}),
    )
}

/// Project VEVENT/VTODO components including recurrence IDs, rules, attendee
/// parameters, RELATED-TO, dates, and nested alarms as structured fields.
/// This view is intentionally not a recurrence expansion or a source serializer.
pub fn calendar(raw: &[u8]) -> Result<Value, String> {
    if raw.len() > 16 * 1024 * 1024 {
        return Err("Calendar resource is too large".into());
    }
    let tree = ical::tree::cst::IcalCst::parse(raw).map_err(|_| "Invalid calendar resource")?;
    if tree.to_bytes() != raw {
        return Err("Calendar must be a single complete resource".into());
    }
    let jcal = tree.decode().to_jcal();
    let text = std::str::from_utf8(raw).map_err(|_| "Calendar resource is not UTF-8")?;
    let calendar: Calendar = text.parse().map_err(|_| "Invalid calendar resource")?;
    let mut components = Vec::new();
    for item in &calendar.components {
        match item {
            CalendarComponent::Event(event) => components.push(component(event, 0)?),
            CalendarComponent::Todo(todo) => components.push(component(todo, 0)?),
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
