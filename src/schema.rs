//! Schema validation for stage payloads.
//!
//! The CLI hands back `structured_output` unvalidated, so a malformed response
//! would otherwise be indistinguishable from a dead agent.

use serde_json::Value;

use crate::OrchError;

/// Compile `schema`, or report why it will not compile.
pub fn validator(schema: &Value) -> Result<jsonschema::Validator, String> {
    jsonschema::validator_for(schema).map_err(|e| e.to_string())
}

/// Validate `data` against `schema`.
///
/// Three outcomes, never collapsed into two: an uncompilable schema used to
/// return `None` — the same `None` a passing payload returns — so a malformed
/// schema silently validated everything and the result went straight to cache.
///
/// - `Ok(Some(msg))` — invalid payload; `msg` is the first error.
/// - `Ok(None)` — the payload satisfies the schema.
/// - `Err(msg)` — the schema itself will not compile.
pub fn schema_error(data: &Value, schema: &Value) -> Result<Option<String>, String> {
    schema_error_with(&validator(schema)?, data)
}

/// Validate against an already-compiled validator.
///
/// `ask` compiles the schema once per call; routing every check through
/// `schema_error` recompiled it — draft detection, meta-schema validation and
/// resource resolution, two to three times for a single stage.
pub fn schema_error_with(
    validator: &jsonschema::Validator,
    data: &Value,
) -> Result<Option<String>, String> {
    let Some(first) = validator.iter_errors(data).next() else {
        return Ok(None);
    };
    let path = first.instance_path().to_string();
    let path = if path.is_empty() || path == "/" {
        "<root>".to_string()
    } else {
        path
    };
    Ok(Some(format!("{path}: {}", first)))
}

/// Input guardrail. Returns `Err(SeamError)` if `data` fails the producer's schema.
///
/// Call this where one stage consumes another's output. Same validator `ask`
/// uses, so a payload is judged by one rule at both ends of the handoff.
///
/// ponytail: duplication until a stage has a second reader. That is the point
/// to call it — not before.
pub fn guard(data: &Value, schema: &Value, label: &str) -> Result<Value, OrchError> {
    match schema_error(data, schema) {
        Err(bad) => Err(OrchError::Schema(format!(
            "stage '{label}' was given a schema that will not compile: {bad}"
        ))),
        Ok(Some(bad)) => Err(OrchError::Seam(format!(
            "stage '{label}' received malformed input: {bad}"
        ))),
        Ok(None) => Ok(data.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj() -> Value {
        json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
        })
    }

    #[test]
    fn accepts_matching_payload() {
        assert_eq!(schema_error(&json!({"answer": "pong"}), &obj()), Ok(None));
    }

    #[test]
    fn reports_path_and_message() {
        let err = schema_error(&json!({"answer": 7}), &obj()).unwrap().unwrap();
        assert!(err.starts_with("/answer:"), "{err}");
    }

    #[test]
    fn reports_root_for_a_missing_required_key() {
        let err = schema_error(&json!({}), &obj()).unwrap().unwrap();
        assert!(err.starts_with("<root>:"), "{err}");
    }

    #[test]
    fn an_uncompilable_schema_is_not_a_passing_payload() {
        // The fail-open this guards: None here once meant "valid", so a broken
        // schema waved every payload through into the cache.
        let broken = json!({"type": "not-a-real-type"});
        assert!(schema_error(&json!({"answer": "pong"}), &broken).is_err());
    }

    #[test]
    fn guard_raises_only_on_bad_input() {
        assert!(guard(&json!({"answer": "x"}), &obj(), "stage").is_ok());
        assert!(matches!(
            guard(&json!({}), &obj(), "stage"),
            Err(OrchError::Seam(_))
        ));
        assert!(matches!(
            guard(&json!({}), &json!({"type": "not-a-real-type"}), "stage"),
            Err(OrchError::Schema(_))
        ));
    }
}