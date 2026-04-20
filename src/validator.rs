use dashmap::DashMap;
use regex::Regex;
use serde::Deserialize;
use sonic_rs::{from_str, JsonValueTrait, Value};
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
    schemas: DashMap<String, DtoSchema>,
    regex_cache: DashMap<String, Regex>,
}

impl ValidatorRegistry {
    pub fn new() -> Self {
        let regex_cache = DashMap::new();
        // Pre-seed with email regex
        if let Ok(re) = Regex::new(
            r"^[a-zA-Z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)*$",
        ) {
            regex_cache.insert("email".to_string(), re);
        }

        Self {
            schemas: DashMap::new(),
            regex_cache,
        }
    }

    pub fn register(&self, name: String, schema_json: &str) -> Result<(), String> {
        let schema: DtoSchema = from_str(schema_json).map_err(|e| e.to_string())?;

        // Pre-compile regexes
        for field in schema.fields.values() {
            for rule in &field.rules {
                if let ValidationRule::Regex { pattern } = rule {
                    self.regex_cache.entry(pattern.clone()).or_insert_with(|| {
                        Regex::new(pattern).expect("Validator pre-compiled regex failed")
                    });
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

        let input: Value = from_str(input_json).map_err(|_| {
            let mut err = HashMap::new();
            err.insert("system".to_string(), vec!["Invalid JSON input".to_string()]);
            err
        })?;

        let mut errors = HashMap::new();
        let mut validated_data: Vec<(Value, Value)> = Vec::new();

        for (name, field) in &schema.fields {
            let value = input.get(name).cloned().or_else(|| {
                if field.has_default {
                    field.default_value.clone()
                } else {
                    None
                }
            });

            match value {
                Some(val) if val.is_null() => {
                    if !field.is_nullable && !field.has_default {
                        errors
                            .entry(name.clone())
                            .or_insert_with(Vec::new)
                            .push(format!("Field '{}' is required.", name));
                    } else {
                        validated_data.push((Value::from(name.as_str()), Value::default()));
                    }
                }
                None => {
                    if !field.is_nullable && !field.has_default {
                        errors
                            .entry(name.clone())
                            .or_insert_with(Vec::new)
                            .push(format!("Field '{}' is required.", name));
                    } else {
                        validated_data.push((Value::from(name.as_str()), Value::default()));
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
                        validated_data.push((Value::from(name.as_str()), val));
                    } else {
                        errors.insert(name.clone(), field_errors);
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(Value::from(&validated_data[..]))
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
                    if let Some(re) = self.regex_cache.get("email") {
                        if !re.is_match(s) {
                            return Err(format!(
                                "The field '{}' must be a valid email address.",
                                field
                            ));
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use sonic_rs::json;

    #[test]
    fn test_email_validation() {
        let registry = ValidatorRegistry::new();
        let rule = ValidationRule::Email;

        assert!(registry
            .check_rule("email", &json!("user@example.com"), &rule)
            .is_ok());
        assert!(registry
            .check_rule("email", &json!("valid.email+alias@domain.co.uk"), &rule)
            .is_ok());

        assert!(registry
            .check_rule("email", &json!("invalid-email"), &rule)
            .is_err());
        assert!(registry
            .check_rule("email", &json!("@example.com"), &rule)
            .is_err());
        assert!(registry
            .check_rule("email", &json!("user@"), &rule)
            .is_err());
    }

    #[test]
    fn test_concurrent_registration() {
        let registry = std::sync::Arc::new(ValidatorRegistry::new());
        let mut handles = vec![];

        for i in 0..10 {
            let reg = std::sync::Arc::clone(&registry);
            handles.push(std::thread::spawn(move || {
                let name = format!("dto_{}", i);
                let schema = r#"{"fields": {"id": {"rules": [{"type": "Numeric"}], "is_nullable": false, "has_default": false}}}"#;
                reg.register(name, schema).unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(registry.schemas.len(), 10);
    }
}
