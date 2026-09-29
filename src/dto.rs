// src/dto.rs
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};

/// Payload for "create or link" transaction endpoints.
///
/// NOTE: `#[serde(untagged)]` cannot discriminate these variants — STAC
/// resources default their `type` field and `#[serde(flatten)]` unknown
/// fields, so `{"id": "x"}` always deserializes as a valid `FullResource`
/// and `LinkReference` would be unreachable. Mode B is instead detected
/// structurally: a JSON object whose ONLY key is a string `id`.
#[derive(Serialize, Debug)]
#[serde(untagged)]
pub enum CreateOrLinkPayload<T> {
    /// Mode A: Complete STAC Resource payload
    FullResource(T),
    /// Mode B: Minimal reference object containing only an ID
    LinkReference { id: String },
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for CreateOrLinkPayload<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(obj) = value.as_object() {
            if obj.len() == 1 {
                if let Some(id) = obj.get("id").and_then(serde_json::Value::as_str) {
                    return Ok(Self::LinkReference { id: id.to_string() });
                }
            }
        }
        serde_json::from_value(value)
            .map(Self::FullResource)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stac::Catalog;

    #[test]
    fn test_mode_b_bare_id_links() {
        let payload: CreateOrLinkPayload<Catalog> =
            serde_json::from_str(r#"{"id":"existing-catalog"}"#).unwrap();
        assert!(matches!(
            payload,
            CreateOrLinkPayload::LinkReference { id } if id == "existing-catalog"
        ));
    }

    #[test]
    fn test_mode_a_full_resource() {
        let payload: CreateOrLinkPayload<Catalog> =
            serde_json::from_str(r#"{"type":"Catalog","id":"new","description":"A new catalog"}"#)
                .unwrap();
        match payload {
            CreateOrLinkPayload::FullResource(c) => assert_eq!(c.id, "new"),
            _ => panic!("expected FullResource"),
        }
    }

    #[test]
    fn test_extra_fields_are_mode_a() {
        // An object with `id` plus extra keys is a (thin) resource, not a link
        let payload: CreateOrLinkPayload<Catalog> =
            serde_json::from_str(r#"{"id":"x","title":"T"}"#).unwrap();
        assert!(matches!(payload, CreateOrLinkPayload::FullResource(_)));
    }
}
