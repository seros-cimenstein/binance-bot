//! Lenient accessors for third-party JSON payloads (DexScreener pairs, Solana
//! transactions, operator-edited files). They mirror Python's `float(x)`,
//! `int(x)` and `str(x)` coercions so malformed fields degrade to defaults
//! instead of failing a whole detection cycle.

use serde_json::{Map, Value};

pub type Object = Map<String, Value>;

/// `float(value)`; `None` when Python would raise.
pub fn float_or_none(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_py_float(s),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// `float(value)` falling back to `0.0`.
pub fn float_or_zero(value: Option<&Value>) -> f64 {
    float_or_none(value).unwrap_or(0.0)
}

/// `int(value)` falling back to `0` (floats truncate toward zero, numeric
/// strings must be integral).
pub fn int_or_zero(value: Option<&Value>) -> i64 {
    int_or_none(value).unwrap_or(0)
}

pub fn int_or_none(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite())
                // Saturating float->int cast is the intended truncation.
                .map(|f| f.trunc() as i64)
        }),
        Value::String(s) => s.trim().replace('_', "").parse::<i64>().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// `str(value)` for scalar payload fields; missing/null become `""` for the
/// `dict.get(key, "")` idiom.
pub fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".to_owned(),
        Some(Value::Bool(false)) => "False".to_owned(),
        Some(other) => other.to_string(),
    }
}

/// The value when it is a JSON string, else `""` (Python `_as_str`).
pub fn str_or_empty(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or("")
}

/// `data.get(key, {}).get(subkey)` tolerating non-object intermediates.
pub fn nested<'a>(data: &'a Object, key: &str, subkey: &str) -> Option<&'a Value> {
    data.get(key)?.as_object()?.get(subkey)
}

/// `data.get(key, {})` when it is an object.
pub fn object<'a>(data: &'a Object, key: &str) -> Option<&'a Object> {
    data.get(key)?.as_object()
}

/// Weight-like numeric field: JSON numbers only (booleans rejected).
pub fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn parse_py_float(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.replace('_', "").parse::<f64>().ok()
}

/// Rebuild objects with lexicographically sorted keys (Python
/// `json.dumps(sort_keys=True)`), independent of serde_json's map backend.
pub fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, sort_keys(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// Keep only object entries of a JSON array (`[x for x in data if isinstance(x, dict)]`).
pub fn objects(items: Vec<Value>) -> Vec<Object> {
    items
        .into_iter()
        .filter_map(|item| match item {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .collect()
}

/// Python `max(items, key=...)`: the FIRST maximal element wins on ties.
pub fn first_max_by<T>(items: impl IntoIterator<Item = T>, key: impl Fn(&T) -> f64) -> Option<T> {
    let mut best: Option<(f64, T)> = None;
    for item in items {
        let k = key(&item);
        let replace = match &best {
            None => true,
            Some((best_k, _)) => k > *best_k,
        };
        if replace {
            best = Some((k, item));
        }
    }
    best.map(|(_, item)| item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coercions_follow_python_semantics() {
        assert_eq!(float_or_zero(Some(&json!("2.5"))), 2.5);
        assert_eq!(float_or_zero(Some(&json!(null))), 0.0);
        assert_eq!(float_or_none(Some(&json!("x"))), None);
        assert_eq!(int_or_zero(Some(&json!(1.9))), 1);
        assert_eq!(int_or_zero(Some(&json!("12"))), 12);
        assert_eq!(int_or_zero(Some(&json!("1.5"))), 0);
        assert_eq!(py_str(Some(&json!(5))), "5");
        assert_eq!(py_str(None), "");
    }

    #[test]
    fn first_max_keeps_first_on_tie() {
        let items = vec![(1, 5.0), (2, 7.0), (3, 7.0)];
        assert_eq!(first_max_by(items, |i| i.1), Some((2, 7.0)));
    }

    #[test]
    fn sort_keys_is_recursive() {
        let v = sort_keys(json!({"b": {"d": 1, "c": 2}, "a": [ {"z": 1, "y": 2} ]}));
        assert_eq!(
            serde_json::to_string(&v).unwrap_or_default(),
            r#"{"a":[{"y":2,"z":1}],"b":{"c":2,"d":1}}"#
        );
    }
}
