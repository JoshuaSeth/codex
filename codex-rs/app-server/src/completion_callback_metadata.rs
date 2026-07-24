use crate::error_code::invalid_request;
use codex_app_server_protocol::JSONRPCErrorError;
use serde_json::Value;

const CALLBACK_METADATA_PROTOCOL: &str = "pitchai-completion-callback/v1";
const MAX_CALLBACK_TEXT_CHARACTERS: usize = 65_536;

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
    if object.len() != 2
        || object.get("protocol_version").and_then(Value::as_str)
            != Some(CALLBACK_METADATA_PROTOCOL)
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn accepts_opaque_metadata_without_a_target() {
        let metadata = json!({
            "protocol_version": CALLBACK_METADATA_PROTOCOL,
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
            "protocol_version": CALLBACK_METADATA_PROTOCOL,
            "text": "Record this completion.",
            "target": {"agent_id": "ori"}
        });
        let valid = json!({
            "protocol_version": CALLBACK_METADATA_PROTOCOL,
            "text": "Record this completion."
        });

        assert!(
            canonical_completion_callback_metadata(Some("work-id"), Some(&with_target)).is_err()
        );
        assert!(canonical_completion_callback_metadata(None, Some(&valid)).is_err());
    }
}
