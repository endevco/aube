pub mod env;
pub mod fs_atomic;
pub mod hash;
pub mod path;
pub mod pkg;
pub mod url;

use serde::{Deserialize, Deserializer};

/// Deserialize npm platform fields (`os`, `cpu`, `libc`) from either a
/// string or an array of strings. Missing fields, nulls, and non-string
/// array entries mean "no constraint" and are dropped.
pub fn string_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::String(s)) => vec![s],
        Some(serde_json::Value::Array(values)) => values
            .into_iter()
            .filter_map(|value| match value {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        Some(_) => Vec::new(),
    })
}
