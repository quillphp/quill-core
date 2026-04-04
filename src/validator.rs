use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ValidationRule {
    Required,
    Email,
    Min { val: f64 },
    Max { val: f64 },
    MinLength { len: usize },
    MaxLength { len: usize },
    Regex { pattern: String },
    Numeric,
    Boolean,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FieldSchema {
    pub rules: Vec<ValidationRule>,
    pub is_nullable: bool,
    pub has_default: bool,
    pub default_value: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DtoSchema {
    pub fields: HashMap<String, FieldSchema>,
}

pub struct ValidatorRegistry {
    schemas: HashMap<String, DtoSchema>,
    regex_cache: HashMap<String, Regex>,
}

impl ValidatorRegistry {
    pub fn new() -> Self {
        Self {
            schemas: HashMap::new(),
            regex_cache: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: String, schema_json: &str) -> Result<(), String> {
        let schema: DtoSchema = serde_json::from_str(schema_json).map_err(|e| e.to_string())?;

        // Pre-compile regexes
        for field in schema.fields.values() {
            for rule in &field.rules {
                if let ValidationRule::Regex { pattern } = rule {
                    if !self.regex_cache.contains_key(pattern) {
                        let re = Regex::new(pattern).map_err(|e| e.to_string())?;
                        self.regex_cache.insert(pattern.clone(), re);
                    }
                }
            }
        }

        self.schemas.insert(name, schema);
        Ok(())
    }

    pub fn validate(
        &self,
        dto_name: &str,
        input_json: &str,
    ) -> Result<Value, HashMap<String, Vec<String>>> {
        let schema = self.schemas.get(dto_name).ok_or_else(|| {
            let mut err = HashMap::new();
            err.insert(
                "system".to_string(),
                vec![format!("DTO '{}' not registered", dto_name)],
            );
            err
        })?;

        let input: Value = serde_json::from_str(input_json).map_err(|_| {
            let mut err = HashMap::new();
            err.insert("system".to_string(), vec!["Invalid JSON input".to_string()]);
            err
        })?;

        let mut errors = HashMap::new();
        let mut validated_data = serde_json::Map::new();

        for (name, field) in &schema.fields {
            let value = input.get(name).cloned().or_else(|| {
                if field.has_default {
                    field.default_value.clone()
                } else {
                    None
                }
            });

            match value {
                Some(Value::Null) | None => {
                    if !field.is_nullable && !field.has_default {
                        errors
                            .entry(name.clone())
                            .or_insert_with(Vec::new)
                            .push(format!("Field '{}' is required.", name));
                    } else {
                        validated_data.insert(name.clone(), Value::Null);
                    }
                }
                Some(val) => {
                    let mut field_errors = Vec::new();
                    for rule in &field.rules {
                        if let Err(e) = self.check_rule(name, &val, rule) {
                            field_errors.push(e);
                        }
                    }

                    if field_errors.is_empty() {
                        validated_data.insert(name.clone(), val);
                    } else {
                        errors.insert(name.clone(), field_errors);
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(Value::Object(validated_data))
        } else {
            Err(errors)
        }
    }

    fn check_rule(&self, field: &str, value: &Value, rule: &ValidationRule) -> Result<(), String> {
        match rule {
            ValidationRule::Required => {
                if value.is_null() {
                    return Err(format!("Field '{}' is required.", field));
                }
            }
            ValidationRule::Email => {
                if let Some(s) = value.as_str() {
                    if !s.contains('@') {
                        // Simple check for now, can use a regex
                        return Err(format!(
                            "The field '{}' must be a valid email address.",
                            field
                        ));
                    }
                }
            }
            ValidationRule::Min { val } => {
                let num = value.as_f64().unwrap_or(0.0);
                if num < *val {
                    return Err(format!("The field '{}' must be at least {}.", field, val));
                }
            }
            ValidationRule::Max { val } => {
                let num = value.as_f64().unwrap_or(0.0);
                if num > *val {
                    return Err(format!(
                        "The field '{}' may not be greater than {}.",
                        field, val
                    ));
                }
            }
            ValidationRule::MinLength { len } => {
                if let Some(s) = value.as_str() {
                    if s.len() < *len {
                        return Err(format!(
                            "The field '{}' must be at least {} characters.",
                            field, len
                        ));
                    }
                }
            }
            ValidationRule::MaxLength { len } => {
                if let Some(s) = value.as_str() {
                    if s.len() > *len {
                        return Err(format!(
                            "The field '{}' may not be greater than {} characters.",
                            field, len
                        ));
                    }
                }
            }
            ValidationRule::Regex { pattern } => {
                if let Some(s) = value.as_str() {
                    if let Some(re) = self.regex_cache.get(pattern) {
                        if !re.is_match(s) {
                            return Err(format!("The field '{}' format is invalid.", field));
                        }
                    }
                }
            }
            ValidationRule::Numeric => {
                if !value.is_number() {
                    return Err(format!("The field '{}' must be a number.", field));
                }
            }
            ValidationRule::Boolean => {
                if !value.is_boolean() {
                    return Err(format!("The field '{}' must be a boolean.", field));
                }
            }
        }
        Ok(())
    }
}
