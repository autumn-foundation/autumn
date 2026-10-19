use serde_json::{Map, Value};
use std::collections::BTreeSet;

pub const FILTERED_PLACEHOLDER: &str = "[FILTERED]";

pub const DEFAULT_FILTER_KEYS: &[&str] = &[
    "password",
    "password_confirmation",
    "token",
    "secret",
    "authorization",
    "api_key",
    "access_token",
    "refresh_token",
    "cookie",
    "set-cookie",
    "ssn",
    "credit_card",
    "card_number",
    "cvv",
];

#[derive(Debug, Clone)]
pub struct ParameterFilter {
    keys: BTreeSet<String>,
    normalized_keys: BTreeSet<String>,
}

impl Default for ParameterFilter {
    fn default() -> Self {
        Self::new(&[], &[])
    }
}

impl ParameterFilter {
    #[must_use]
    pub fn new(additional: &[String], opt_out_defaults: &[String]) -> Self {
        let opt_out_defaults: BTreeSet<String> = opt_out_defaults
            .iter()
            .filter_map(|key| normalize_key(key))
            .collect();

        let mut keys = BTreeSet::new();
        for key in DEFAULT_FILTER_KEYS {
            let Some(normalized_default) = normalize_key(key) else {
                continue;
            };
            if !opt_out_defaults.contains(&normalized_default) {
                keys.insert(key.to_ascii_lowercase());
            }
        }
        for key in additional {
            if let Some(key) = normalize_key(key) {
                keys.insert(key);
            }
        }

        let normalized_keys = keys
            .iter()
            .filter_map(|k| normalize_key(k))
            .collect::<BTreeSet<_>>();

        Self {
            keys,
            normalized_keys,
        }
    }

    #[must_use]
    pub fn scrub_json(&self, value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(self.scrub_map(map)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.scrub_json(v)).collect()),
            _ => value.clone(),
        }
    }

    fn scrub_map(&self, map: &Map<String, Value>) -> Map<String, Value> {
        let mut out = Map::new();
        for (key, value) in map {
            if self.matches_key(key) {
                out.insert(key.clone(), Value::String(FILTERED_PLACEHOLDER.to_owned()));
            } else {
                out.insert(key.clone(), self.scrub_json(value));
            }
        }
        out
    }

    #[must_use]
    pub fn matches_key(&self, key: &str) -> bool {
        // `normalize_key` keeps a key's ASCII alphanumerics, lowercased. A
        // non-ASCII char's UTF-8 bytes are all >= 0x80, so filtering the bytes
        // keeps exactly the same characters, and the normalized key can be
        // built on the stack rather than in a fresh `String` per probe: this
        // runs once per field of every captured log event and scrubbed map.
        let mut buf = [0u8; INLINE_KEY_CAPACITY];
        let mut len = 0;
        for byte in key.bytes().filter(u8::is_ascii_alphanumeric) {
            let Some(slot) = buf.get_mut(len) else {
                // Longer than the inline buffer: take the allocating path.
                return normalize_key(key)
                    .is_some_and(|normalized| self.contains_normalized(&normalized));
            };
            *slot = byte.to_ascii_lowercase();
            len += 1;
        }
        // An empty normalized key matches nothing, as `normalize_key`'s `None`
        // does. Every byte is ASCII, so the slice is always valid UTF-8.
        len > 0
            && buf
                .get(..len)
                .and_then(|normalized| std::str::from_utf8(normalized).ok())
                .is_some_and(|normalized| self.contains_normalized(normalized))
    }

    fn contains_normalized(&self, normalized: &str) -> bool {
        self.keys.contains(normalized) || self.normalized_keys.contains(normalized)
    }
}

/// Inline capacity of the stack buffer [`ParameterFilter::matches_key`]
/// normalizes into. Every default key fits with room to spare; a longer input
/// falls back to the allocating `normalize_key`.
const INLINE_KEY_CAPACITY: usize = 64;

fn normalize_key(key: &str) -> Option<String> {
    let normalized: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

#[must_use]
pub fn normalized_opt_out_defaults(opt_out_defaults: &[String]) -> Vec<String> {
    let defaults: BTreeSet<String> = DEFAULT_FILTER_KEYS
        .iter()
        .filter_map(|key| normalize_key(key))
        .collect();

    let mut result: Vec<String> = opt_out_defaults
        .iter()
        .filter_map(|key| normalize_key(key))
        .filter(|key| defaults.contains(key))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    result.sort();
    result
}

#[must_use]
pub fn scrub(value: &Value) -> Value {
    ParameterFilter::default().scrub_json(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn red_default_password_is_scrubbed() {
        let payload = json!({"password":"hunter2","email":"user@example.com"});
        let out = scrub(&payload);
        assert_eq!(out["password"], FILTERED_PLACEHOLDER);
        assert_eq!(out["email"], "user@example.com");
    }

    #[test]
    fn matching_is_case_insensitive_and_exact() {
        let filter = ParameterFilter::default();
        assert!(filter.matches_key("PASSWORD"));
        assert!(!filter.matches_key("customer_password"));
        assert!(!filter.matches_key("assignment"));
        assert!(!filter.matches_key("broken"));
    }

    #[test]
    fn additive_custom_key_and_opt_out() {
        let filter = ParameterFilter::new(&["pin".to_owned()], &["password".to_owned()]);
        let payload = json!({"password":"open","pin":"1234"});
        let out = filter.scrub_json(&payload);
        assert_eq!(out["password"], "open");
        assert_eq!(out["pin"], FILTERED_PLACEHOLDER);
    }

    #[test]
    fn empty_custom_key_does_not_scrub_everything() {
        let filter = ParameterFilter::new(&[String::new()], &[]);
        assert!(!filter.matches_key("email"));
        assert!(!filter.matches_key("anything"));
    }

    #[test]
    fn reports_opted_out_defaults() {
        let opts = vec![
            "PASSWORD".to_owned(),
            "missing".to_owned(),
            "apiKey".to_owned(),
        ];
        let normalized = normalized_opt_out_defaults(&opts);
        assert_eq!(normalized, vec!["apikey".to_owned(), "password".to_owned()]);
    }

    #[test]
    fn api_key_variants_match_after_normalization() {
        let filter = ParameterFilter::default();
        assert!(filter.matches_key("api_key"));
        assert!(filter.matches_key("apiKey"));
        assert!(filter.matches_key("apikey"));
        assert!(filter.matches_key("API-KEY"));
    }

    /// The stack-buffer normalization in `matches_key` must agree with the
    /// allocating `normalize_key` it replaced on the hot path, including
    /// non-ASCII input, empty results and keys past the inline capacity.
    #[test]
    fn matches_key_agrees_with_allocating_normalization() {
        let long_custom = "k".repeat(70);
        let filter = ParameterFilter::new(
            &["x-real-ip".to_owned(), long_custom.clone()],
            &["cookie".to_owned()],
        );
        let at_cap = "a".repeat(INLINE_KEY_CAPACITY);
        let at_cap_with_noise = format!("{}-_-", "B".repeat(INLINE_KEY_CAPACITY));
        let over_cap = format!("{}password", "a".repeat(80));
        let long_upper = "K".repeat(70);
        for key in [
            "password",
            "PASSWORD",
            "Pass_Word",
            "password_confirmation",
            "set-cookie",
            "api-key",
            "apiKey",
            "x_real_ip",
            "X-Real-IP",
            "cookie",
            "email",
            "",
            "___",
            "p\u{e4}ssword",
            "pass\u{301}word",
            "\u{130}token",
            at_cap.as_str(),
            at_cap_with_noise.as_str(),
            over_cap.as_str(),
            long_upper.as_str(),
            long_custom.as_str(),
        ] {
            let expected = normalize_key(key).is_some_and(|normalized| {
                filter.keys.contains(&normalized) || filter.normalized_keys.contains(&normalized)
            });
            assert_eq!(filter.matches_key(key), expected, "key {key:?}");
        }

        // Spot-check the expected answers, so the comparison above cannot
        // pass by both sides being wrong in the same way.
        assert!(filter.matches_key("Pass_Word"));
        assert!(filter.matches_key("X-Real-IP"));
        assert!(filter.matches_key(&long_upper));
        assert!(!filter.matches_key("cookie"));
        assert!(!filter.matches_key("p\u{e4}ssword"));
        assert!(!filter.matches_key(""));
        assert!(!filter.matches_key(&over_cap));
    }
}
