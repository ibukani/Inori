#[rustfmt::skip]
mod generated;
pub use generated::*;

pub fn compatible(major: u64) -> bool {
    major == PROTOCOL_MAJOR
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extension_payload_is_not_limited_to_text() {
        let event = EventEnvelope {
            protocol_version: PROTOCOL_VERSION.into(),
            id: "e".into(),
            kind: "example.sensor.pose".into(),
            session_id: None,
            generation_id: None,
            audience: Audience::Private,
            payload: serde_json::json!({"pose": [0.2, 0.3], "resource": {"mime": "image/png", "id": "opaque-resource-id"}}),
        };
        let decoded: EventEnvelope =
            serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(decoded.payload["pose"][0], 0.2);
        assert!(!compatible(2));
    }
}
