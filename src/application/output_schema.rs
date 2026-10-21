//! Declarative, dependency-light JSON output contracts for app manifests.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{DustError, Result};

use super::validation::{ValidationDecision, ValidationEvidence};

const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_SCHEMA_NODES: usize = 4096;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_ISSUES: usize = 8;
const MAX_TRACKED_ISSUES: usize = MAX_ISSUES + 1;
const MAX_REASON_BYTES: usize = 2048;
const MAX_PATTERN_BYTES: usize = 512;

/// Supported JSON value types in an app's `output_schema` contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputType {
    Object,
    Array,
    String,
    Number,
    Integer,
    Boolean,
    Null,
}

/// A deliberately bounded JSON-Schema-like subset. Unknown keywords fail
/// manifest parsing instead of being silently ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSchema {
    #[serde(rename = "type")]
    pub schema_type: OutputType,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, OutputSchema>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
    #[serde(
        rename = "additionalProperties",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_properties: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<OutputSchema>>,
    #[serde(rename = "enum", default, skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    #[serde(
        rename = "exclusiveMinimum",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exclusive_minimum: Option<f64>,
    #[serde(
        rename = "exclusiveMaximum",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exclusive_maximum: Option<f64>,
    #[serde(rename = "minLength", default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<usize>,
    #[serde(rename = "maxLength", default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(rename = "minItems", default, skip_serializing_if = "Option::is_none")]
    pub min_items: Option<usize>,
    #[serde(rename = "maxItems", default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<usize>,
}

impl OutputSchema {
    /// Validate the declaration itself before using it against model output.
    pub fn validate(&self) -> Result<()> {
        let mut remaining_nodes = MAX_SCHEMA_NODES;
        self.validate_at("$", 0, &mut remaining_nodes)
    }

    fn validate_at(&self, path: &str, depth: usize, remaining_nodes: &mut usize) -> Result<()> {
        if depth > MAX_SCHEMA_DEPTH || *remaining_nodes == 0 {
            return Err(DustError::Config(format!(
                "output_schema exceeds its depth or node limit at {path}"
            )));
        }
        *remaining_nodes -= 1;

        if let Some(values) = &self.enum_values
            && (values.is_empty() || values.len() > 256)
        {
            return Err(DustError::Config(format!(
                "output_schema enum at {path} must contain between 1 and 256 values"
            )));
        }
        if self.enum_values.as_ref().is_some_and(|values| {
            !values
                .iter()
                .any(|value| matches_type(value, self.schema_type))
        }) {
            return Err(DustError::Config(format!(
                "output_schema enum has no value matching type={} at {path}",
                type_name(self.schema_type)
            )));
        }
        if self.properties.len() > 256 {
            return Err(DustError::Config(format!(
                "output_schema has too many properties at {path}"
            )));
        }
        if self.required.len() > 256 {
            return Err(DustError::Config(format!(
                "output_schema has too many required names at {path}"
            )));
        }
        let mut required = std::collections::BTreeSet::new();
        for name in &self.required {
            if name.is_empty() || name.len() > 256 || name.contains('\0') || !required.insert(name)
            {
                return Err(DustError::Config(format!(
                    "output_schema required names at {path} must be unique, nonempty, and at most 256 bytes"
                )));
            }
            if self.additional_properties == Some(false) && !self.properties.contains_key(name) {
                return Err(DustError::Config(format!(
                    "output_schema requires undeclared property '{name}' while additionalProperties is false at {path}"
                )));
            }
        }

        let is_number = matches!(self.schema_type, OutputType::Number | OutputType::Integer);
        if (!self.properties.is_empty()
            || !self.required.is_empty()
            || self.additional_properties.is_some())
            && self.schema_type != OutputType::Object
        {
            return Err(DustError::Config(format!(
                "object constraints require type=object at {path}"
            )));
        }
        if self.items.is_some() && self.schema_type != OutputType::Array {
            return Err(DustError::Config(format!(
                "items requires type=array at {path}"
            )));
        }
        if (self.minimum.is_some()
            || self.maximum.is_some()
            || self.exclusive_minimum.is_some()
            || self.exclusive_maximum.is_some())
            && !is_number
        {
            return Err(DustError::Config(format!(
                "numeric constraints require type=number or integer at {path}"
            )));
        }
        if (self.min_length.is_some() || self.max_length.is_some() || self.pattern.is_some())
            && self.schema_type != OutputType::String
        {
            return Err(DustError::Config(format!(
                "string constraints require type=string at {path}"
            )));
        }
        if (self.min_items.is_some() || self.max_items.is_some())
            && self.schema_type != OutputType::Array
        {
            return Err(DustError::Config(format!(
                "array constraints require type=array at {path}"
            )));
        }
        if self
            .min_length
            .zip(self.max_length)
            .is_some_and(|(min, max)| min > max)
            || self
                .min_items
                .zip(self.max_items)
                .is_some_and(|(min, max)| min > max)
        {
            return Err(DustError::Config(format!(
                "output_schema minimum exceeds maximum at {path}"
            )));
        }

        let numeric_bounds = [
            self.minimum,
            self.maximum,
            self.exclusive_minimum,
            self.exclusive_maximum,
        ];
        if numeric_bounds
            .iter()
            .flatten()
            .any(|bound| !bound.is_finite())
            || self
                .minimum
                .zip(self.maximum)
                .is_some_and(|(min, max)| min > max)
            || self
                .exclusive_minimum
                .zip(self.exclusive_maximum)
                .is_some_and(|(min, max)| min >= max)
        {
            return Err(DustError::Config(format!(
                "output_schema numeric bounds are invalid at {path}"
            )));
        }
        let lower_bound = [
            self.minimum.map(|value| (value, false)),
            self.exclusive_minimum.map(|value| (value, true)),
        ]
        .into_iter()
        .flatten()
        .max_by(|left, right| left.0.total_cmp(&right.0));
        let upper_bound = [
            self.maximum.map(|value| (value, false)),
            self.exclusive_maximum.map(|value| (value, true)),
        ]
        .into_iter()
        .flatten()
        .min_by(|left, right| left.0.total_cmp(&right.0));
        if lower_bound.zip(upper_bound).is_some_and(|(lower, upper)| {
            lower.0 > upper.0 || (lower.0 == upper.0 && (lower.1 || upper.1))
        }) {
            return Err(DustError::Config(format!(
                "output_schema numeric bounds have no valid value at {path}"
            )));
        }

        if let Some(pattern) = &self.pattern {
            if pattern.len() > MAX_PATTERN_BYTES {
                return Err(DustError::Config(format!(
                    "output_schema pattern exceeds {MAX_PATTERN_BYTES} bytes at {path}"
                )));
            }
            regex::RegexBuilder::new(pattern)
                .size_limit(1024 * 1024)
                .dfa_size_limit(1024 * 1024)
                .build()
                .map_err(|error| {
                    DustError::Config(format!("invalid output_schema pattern at {path}: {error}"))
                })?;
        }

        for (name, property) in &self.properties {
            if name.is_empty() || name.len() > 256 || name.contains('\0') {
                return Err(DustError::Config(format!(
                    "output_schema property name at {path} must be nonempty and at most 256 bytes"
                )));
            }
            property.validate_at(
                &format!("{path}/{}", pointer_escape(name)),
                depth + 1,
                remaining_nodes,
            )?;
        }
        if let Some(items) = &self.items {
            items.validate_at(&format!("{path}/*"), depth + 1, remaining_nodes)?;
        }
        Ok(())
    }

    /// Check that the final response is a single JSON value satisfying this
    /// contract. Failures are returned as bounded feedback for the model.
    pub fn validate_output(&self, output: &str) -> Result<ValidationEvidence> {
        self.validate()?;
        let issues = if output.trim().is_empty() {
            vec!["$: output is empty; expected a JSON value".to_owned()]
        } else if output.len() > MAX_OUTPUT_BYTES {
            vec![format!(
                "$: output exceeds the {MAX_OUTPUT_BYTES}-byte JSON validation limit"
            )]
        } else {
            match serde_json::from_str::<Value>(output) {
                Ok(value) => {
                    let mut issues = Vec::new();
                    self.check_value(&value, "$", &mut issues);
                    issues
                }
                Err(error) => vec![format!(
                    "$: output is not valid JSON (line {}, column {})",
                    error.line(),
                    error.column()
                )],
            }
        };

        if issues.is_empty() {
            return Ok(ValidationEvidence {
                passed: true,
                reason: "Output matches the declared JSON schema".into(),
                exit_code: None,
                decision: Some(ValidationDecision::Complete),
            });
        }

        let shown = issues
            .iter()
            .take(MAX_ISSUES)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let omitted = issues.len().saturating_sub(MAX_ISSUES);
        let reason = if omitted > 0 {
            format!(
                "Output does not satisfy output_schema:\n{shown}\nAdditional issues were omitted"
            )
        } else {
            format!("Output does not satisfy output_schema:\n{shown}")
        };
        Ok(ValidationEvidence {
            passed: false,
            reason: truncate(&reason, MAX_REASON_BYTES),
            exit_code: None,
            decision: Some(ValidationDecision::Continue),
        })
    }

    fn check_value(&self, value: &Value, path: &str, issues: &mut Vec<String>) {
        if issues.len() >= MAX_TRACKED_ISSUES {
            return;
        }
        if !matches_type(value, self.schema_type) {
            push_issue(
                issues,
                format!(
                    "{path}: expected {}, got {}",
                    type_name(self.schema_type),
                    value_type_name(value)
                ),
            );
            return;
        }
        if let Some(allowed) = &self.enum_values
            && !allowed.contains(value)
        {
            push_issue(
                issues,
                format!(
                    "{path}: value is not one of the {} allowed values",
                    allowed.len()
                ),
            );
        }

        match value {
            Value::Object(object) => {
                for required in &self.required {
                    if !object.contains_key(required) {
                        push_issue(
                            issues,
                            format!(
                                "{path}/{}: required property is missing",
                                pointer_escape(required)
                            ),
                        );
                    }
                }
                for (name, child_schema) in &self.properties {
                    if let Some(child) = object.get(name) {
                        child_schema.check_value(
                            child,
                            &format!("{path}/{}", pointer_escape(name)),
                            issues,
                        );
                    }
                }
                if self.additional_properties == Some(false) {
                    for name in object.keys() {
                        if !self.properties.contains_key(name) {
                            push_issue(
                                issues,
                                format!(
                                    "{path}/{}: additional property is not allowed",
                                    pointer_escape(name)
                                ),
                            );
                        }
                    }
                }
            }
            Value::Array(array) => {
                if self.min_items.is_some_and(|min| array.len() < min) {
                    push_issue(
                        issues,
                        format!(
                            "{path}: expected at least {} array item(s), got {}",
                            self.min_items.unwrap_or_default(),
                            array.len()
                        ),
                    );
                }
                if self.max_items.is_some_and(|max| array.len() > max) {
                    push_issue(
                        issues,
                        format!(
                            "{path}: expected at most {} array item(s), got {}",
                            self.max_items.unwrap_or_default(),
                            array.len()
                        ),
                    );
                }
                if let Some(item_schema) = &self.items {
                    for (index, item) in array.iter().enumerate() {
                        if issues.len() >= MAX_TRACKED_ISSUES {
                            break;
                        }
                        item_schema.check_value(item, &format!("{path}/{index}"), issues);
                    }
                }
            }
            Value::String(string) => {
                let length = string.chars().count();
                if self.min_length.is_some_and(|min| length < min) {
                    push_issue(
                        issues,
                        format!(
                            "{path}: string must contain at least {} character(s), got {length}",
                            self.min_length.unwrap_or_default()
                        ),
                    );
                }
                if self.max_length.is_some_and(|max| length > max) {
                    push_issue(
                        issues,
                        format!(
                            "{path}: string must contain at most {} character(s), got {length}",
                            self.max_length.unwrap_or_default()
                        ),
                    );
                }
                if let Some(pattern) = &self.pattern
                    && !regex::Regex::new(pattern)
                        .expect("pattern is checked when the manifest is validated")
                        .is_match(string)
                {
                    push_issue(
                        issues,
                        format!("{path}: string does not match the required pattern"),
                    );
                }
            }
            Value::Number(number) => {
                if let Some(number) = number.as_f64() {
                    if self.minimum.is_some_and(|min| number < min) {
                        push_issue(
                            issues,
                            format!(
                                "{path}: number must be at least {}",
                                self.minimum.unwrap_or_default()
                            ),
                        );
                    }
                    if self.maximum.is_some_and(|max| number > max) {
                        push_issue(
                            issues,
                            format!(
                                "{path}: number must be at most {}",
                                self.maximum.unwrap_or_default()
                            ),
                        );
                    }
                    if self.exclusive_minimum.is_some_and(|min| number <= min) {
                        push_issue(
                            issues,
                            format!(
                                "{path}: number must be greater than {}",
                                self.exclusive_minimum.unwrap_or_default()
                            ),
                        );
                    }
                    if self.exclusive_maximum.is_some_and(|max| number >= max) {
                        push_issue(
                            issues,
                            format!(
                                "{path}: number must be less than {}",
                                self.exclusive_maximum.unwrap_or_default()
                            ),
                        );
                    }
                }
            }
            Value::Bool(_) | Value::Null => {}
        }
    }
}

fn matches_type(value: &Value, expected: OutputType) -> bool {
    match expected {
        OutputType::Object => value.is_object(),
        OutputType::Array => value.is_array(),
        OutputType::String => value.is_string(),
        OutputType::Number => value.is_number(),
        OutputType::Integer => value.as_f64().is_some_and(|value| value.fract() == 0.0),
        OutputType::Boolean => value.is_boolean(),
        OutputType::Null => value.is_null(),
    }
}

fn type_name(value: OutputType) -> &'static str {
    match value {
        OutputType::Object => "object",
        OutputType::Array => "array",
        OutputType::String => "string",
        OutputType::Number => "number",
        OutputType::Integer => "integer",
        OutputType::Boolean => "boolean",
        OutputType::Null => "null",
    }
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.as_f64().is_some_and(|number| number.fract() == 0.0) => {
            "integer"
        }
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn push_issue(issues: &mut Vec<String>, issue: String) {
    if issues.len() < MAX_ISSUES {
        issues.push(issue);
    } else if issues.len() == MAX_ISSUES {
        issues.push("additional validation issue omitted".into());
    }
}

fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(value: Value) -> OutputSchema {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn checks_json_type_shape_and_value_constraints() {
        let contract = schema(serde_json::json!({
            "type":"object",
            "required":["title", "count", "items"],
            "additionalProperties":false,
            "properties":{
                "title":{"type":"string","minLength":2,"pattern":"^[A-Z]"},
                "count":{"type":"integer","minimum":1,"maximum":5},
                "items":{"type":"array","minItems":1,"items":{"type":"boolean"}}
            }
        }));
        contract.validate().unwrap();

        let failed = contract
            .validate_output(r#"{"title":"x","count":8,"items":["yes"],"extra":true}"#)
            .unwrap();
        assert!(!failed.passed);
        assert_eq!(failed.decision, Some(ValidationDecision::Continue));
        assert!(
            failed
                .reason
                .contains("$/title: string must contain at least 2")
        );
        assert!(failed.reason.contains("$/count: number must be at most 5"));
        assert!(
            failed
                .reason
                .contains("$/items/0: expected boolean, got string")
        );
        assert!(
            failed
                .reason
                .contains("$/extra: additional property is not allowed")
        );
    }

    #[test]
    fn rejects_non_json_and_accepts_only_complete_json_values() {
        let contract = schema(serde_json::json!({"type":"object"}));
        for output in ["", "not json", "```json\n{}\n```", "{} trailing"] {
            let evidence = contract.validate_output(output).unwrap();
            assert!(!evidence.passed, "accepted {output:?}");
            assert_eq!(evidence.decision, Some(ValidationDecision::Continue));
        }
        let valid = contract.validate_output("{\"ok\":true}").unwrap();
        assert!(valid.passed);
        assert_eq!(valid.decision, Some(ValidationDecision::Complete));
    }

    #[test]
    fn checks_required_enum_and_pattern_constraints() {
        let contract = schema(serde_json::json!({
            "type":"object",
            "required":["name", "status"],
            "properties":{
                "name":{"type":"string", "pattern":"^[A-Z][a-z]+$"},
                "status":{"type":"string", "enum":["ready", "done"]}
            }
        }));
        let evidence = contract
            .validate_output(r#"{"name":"lowercase","status":"waiting"}"#)
            .unwrap();
        assert!(
            evidence
                .reason
                .contains("does not match the required pattern")
        );
        assert!(evidence.reason.contains("not one of the 2 allowed values"));

        let missing = contract.validate_output(r#"{"name":"Dust"}"#).unwrap();
        assert!(
            missing
                .reason
                .contains("$/status: required property is missing")
        );
    }

    #[test]
    fn rejects_invalid_or_unsupported_contracts() {
        assert!(
            serde_json::from_value::<OutputSchema>(serde_json::json!({
                "type":"object", "mysteryKeyword":true
            }))
            .is_err()
        );
        let invalid = schema(serde_json::json!({
            "type":"string", "minLength":4, "maxLength":2
        }));
        assert!(invalid.validate().is_err());
        let invalid_pattern = schema(serde_json::json!({"type":"string", "pattern":"("}));
        assert!(invalid_pattern.validate().is_err());
    }
}
