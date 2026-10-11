//! Typed environment readers.
//!
//! Every reader accepts several names (first non-blank wins) so legacy aliases
//! keep working, and fails with a [`ConfigError`] naming the variable instead of
//! silently falling back on malformed input. Blank values count as unset.

use std::str::FromStr;

use rust_decimal::Decimal;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("missing required environment variable: {0}")]
    Missing(String),
    #[error("{name} must be {expected}, got {value:?}")]
    Invalid {
        name: String,
        expected: &'static str,
        value: String,
    },
}

/// Load `.env` from the working directory if present. Existing process
/// variables win over file values, matching python-dotenv's default.
pub fn load_dotenv() {
    // A missing .env is the normal container case; malformed files are reported.
    if let Err(err) = dotenvy::dotenv()
        && !err.not_found()
    {
        tracing::warn!(error = %err, "ignoring unreadable .env file");
    }
}

/// First non-blank value among `names`, trimmed.
pub fn raw(names: &[&str]) -> Option<(String, String)> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .map(|v| ((*name).to_owned(), v))
    })
}

fn joined(names: &[&str]) -> String {
    names.join(" / ")
}

pub fn string(names: &[&str], default: &str) -> String {
    raw(names).map_or_else(|| default.to_owned(), |(_, v)| v)
}

pub fn optional_string(names: &[&str]) -> Option<String> {
    raw(names).map(|(_, v)| v)
}

pub fn required(names: &[&str]) -> Result<String, ConfigError> {
    raw(names)
        .map(|(_, v)| v)
        .ok_or_else(|| ConfigError::Missing(joined(names)))
}

/// Accepts `1/0`, `true/false`, `yes/no`, `on/off` (case-insensitive).
pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

pub fn boolean(names: &[&str], default: bool) -> Result<bool, ConfigError> {
    match raw(names) {
        None => Ok(default),
        Some((name, value)) => parse_bool(&value).ok_or(ConfigError::Invalid {
            name,
            expected: "a boolean",
            value,
        }),
    }
}

/// Explicit operator confirmation: only the literal `yes` (case-insensitive) counts.
pub fn confirmed(names: &[&str]) -> bool {
    raw(names).is_some_and(|(_, v)| v.eq_ignore_ascii_case("yes"))
}

fn parsed<T: FromStr>(names: &[&str], expected: &'static str) -> Result<Option<T>, ConfigError> {
    match raw(names) {
        None => Ok(None),
        Some((name, value)) => value
            .parse::<T>()
            .map(Some)
            .map_err(|_| ConfigError::Invalid {
                name,
                expected,
                value,
            }),
    }
}

pub fn decimal(names: &[&str], default: Decimal) -> Result<Decimal, ConfigError> {
    Ok(optional_decimal(names)?.unwrap_or(default))
}

pub fn optional_decimal(names: &[&str]) -> Result<Option<Decimal>, ConfigError> {
    match raw(names) {
        None => Ok(None),
        Some((name, value)) => Decimal::from_str_exact(&value)
            .or_else(|_| Decimal::from_scientific(&value))
            .map(Some)
            .map_err(|_| ConfigError::Invalid {
                name,
                expected: "a decimal number",
                value,
            }),
    }
}

pub fn float(names: &[&str], default: f64) -> Result<f64, ConfigError> {
    let value = parsed::<f64>(names, "a number")?.unwrap_or(default);
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ConfigError::Invalid {
            name: joined(names),
            expected: "a finite number",
            value: value.to_string(),
        })
    }
}

pub fn optional_float(names: &[&str]) -> Result<Option<f64>, ConfigError> {
    parsed::<f64>(names, "a number")
}

pub fn integer<T: FromStr>(names: &[&str], default: T) -> Result<T, ConfigError> {
    Ok(parsed::<T>(names, "an integer")?.unwrap_or(default))
}

pub fn optional_integer<T: FromStr>(names: &[&str]) -> Result<Option<T>, ConfigError> {
    parsed::<T>(names, "an integer")
}

/// Comma/whitespace separated list, trimmed, blanks dropped.
pub fn list(names: &[&str]) -> Vec<String> {
    raw(names)
        .map(|(_, v)| {
            v.split([',', ' ', '\n', '\t'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Like [`list`] but upper-cased (asset/symbol lists).
pub fn upper_list(names: &[&str]) -> Vec<String> {
    list(names)
        .into_iter()
        .map(|s| s.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_parsing_accepts_common_spellings_and_rejects_garbage() {
        assert_eq!(parse_bool(" YES "), Some(true));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }
}
