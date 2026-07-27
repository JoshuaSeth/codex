use crate::error_code::invalid_request;
use codex_app_server_protocol::JSONRPCErrorError;
use serde_json::Value;
use uuid::Uuid;

const CALLBACK_METADATA_PROTOCOL_V1: &str = "pitchai-completion-callback/v1";
const CALLBACK_METADATA_PROTOCOL_V2: &str = "pitchai-completion-callback/v2";
const MAX_CALLBACK_TEXT_CHARACTERS: usize = 65_536;
const MAX_CALLBACK_CONTEXT_CHARACTERS: usize = 512;
const CALLBACK_CONTEXT_FIELDS: [&str; 8] = [
    "source_agent_id",
    "project_id",
    "project_title",
    "command_work_id",
    "origin_actor_kind",
    "origin_agent_id",
    "origin_source_ref_kind",
    "origin_source_ref_id",
];
const ORIGIN_ACTOR_KINDS: [&str; 6] = ["human", "voice", "agent", "service", "system", "unknown"];

pub(crate) fn canonical_completion_callback_metadata(
    completion_work_id: Option<&str>,
    metadata: Option<&Value>,
) -> Result<String, JSONRPCErrorError> {
    let Some(metadata) = metadata else {
        return Ok(String::new());
    };
    if completion_work_id.is_none() {
        return Err(invalid_request(
            "completionCallbackMetadata requires completionWorkId",
        ));
    }
    let object = metadata
        .as_object()
        .ok_or_else(|| invalid_request("completionCallbackMetadata must be a JSON object"))?;
    let protocol_version = object.get("protocol_version").and_then(Value::as_str);
    let valid_shape = match protocol_version {
        Some(CALLBACK_METADATA_PROTOCOL_V1) => object.len() == 2,
        Some(CALLBACK_METADATA_PROTOCOL_V2) => {
            object.len() == 3
                && object
                    .get("context")
                    .is_some_and(valid_completion_callback_context)
        }
        _ => false,
    };
    if !valid_shape {
        return Err(invalid_request(
            "completionCallbackMetadata fields do not match the producer contract",
        ));
    }
    let text = object
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_request("completionCallbackMetadata.text must be a string"))?;
    if text.trim().is_empty() || text.chars().count() > MAX_CALLBACK_TEXT_CHARACTERS {
        return Err(invalid_request(format!(
            "completionCallbackMetadata.text must be nonblank and at most \
             {MAX_CALLBACK_TEXT_CHARACTERS} characters"
        )));
    }
    serde_json::to_string(metadata).map_err(|err| {
        invalid_request(format!(
            "completionCallbackMetadata could not be serialized: {err}"
        ))
    })
}

fn valid_completion_callback_context(value: &Value) -> bool {
    let Some(context) = value.as_object() else {
        return false;
    };
    if context.len() != CALLBACK_CONTEXT_FIELDS.len()
        || CALLBACK_CONTEXT_FIELDS
            .iter()
            .any(|field| !context.contains_key(*field))
    {
        return false;
    }
    let required_fields = ["source_agent_id", "command_work_id", "origin_actor_kind"];
    let optional_fields = [
        "project_id",
        "project_title",
        "origin_agent_id",
        "origin_source_ref_kind",
        "origin_source_ref_id",
    ];
    required_fields
        .iter()
        .all(|field| context.get(*field).is_some_and(valid_context_text))
        && optional_fields.iter().all(|field| {
            context
                .get(*field)
                .is_some_and(|value| value.is_null() || valid_context_text(value))
        })
        && context
            .get("command_work_id")
            .and_then(Value::as_str)
            .is_some_and(canonical_uuid)
        && context
            .get("origin_actor_kind")
            .and_then(Value::as_str)
            .is_some_and(|actor_kind| ORIGIN_ACTOR_KINDS.contains(&actor_kind))
}

fn valid_context_text(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        !text.is_empty()
            && text.trim() == text
            && text.chars().count() <= MAX_CALLBACK_CONTEXT_CHARACTERS
            && !text
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '\t'))
    })
}

fn canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn accepts_opaque_metadata_without_a_target() {
        let metadata = json!({
            "protocol_version": CALLBACK_METADATA_PROTOCOL_V1,
            "text": "Record this completion."
        });

        let canonical = canonical_completion_callback_metadata(Some("work-id"), Some(&metadata))
            .expect("valid metadata");

        assert_eq!(
            canonical,
            serde_json::to_string(&metadata).expect("serialize")
        );
    }

    #[test]
    fn rejects_routing_fields_and_missing_work_identity() {
        let with_target = json!({
            "protocol_version": CALLBACK_METADATA_PROTOCOL_V1,
            "text": "Record this completion.",
            "target": {"agent_id": "ori"}
        });
        let valid = json!({
            "protocol_version": CALLBACK_METADATA_PROTOCOL_V1,
            "text": "Record this completion."
        });

        assert!(
            canonical_completion_callback_metadata(Some("work-id"), Some(&with_target)).is_err()
        );
        assert!(canonical_completion_callback_metadata(None, Some(&valid)).is_err());
    }

    #[test]
    fn accepts_v2_source_context_without_a_destination() {
        let metadata = json!({
            "protocol_version": CALLBACK_METADATA_PROTOCOL_V2,
            "text": "Record this completion.",
            "context": {
                "source_agent_id": "worker",
                "project_id": "pitchai_infrastructure",
                "project_title": "PitchAI Infrastructure",
                "command_work_id": "10000000-0000-0000-0000-000000000001",
                "origin_actor_kind": "agent",
                "origin_agent_id": "ori",
                "origin_source_ref_kind": "codex_thread",
                "origin_source_ref_id": "20000000-0000-0000-0000-000000000001",
            }
        });

        assert!(canonical_completion_callback_metadata(Some("work-id"), Some(&metadata)).is_ok());
    }

    #[test]
    fn rejects_v2_invalid_command_identity_or_actor_kind() {
        let metadata = json!({
            "protocol_version": CALLBACK_METADATA_PROTOCOL_V2,
            "text": "Record this completion.",
            "context": {
                "source_agent_id": "worker",
                "project_id": null,
                "project_title": null,
                "command_work_id": "not-a-uuid",
                "origin_actor_kind": "client",
                "origin_agent_id": null,
                "origin_source_ref_kind": null,
                "origin_source_ref_id": null,
            }
        });

        assert!(canonical_completion_callback_metadata(Some("work-id"), Some(&metadata)).is_err());
    }
}
