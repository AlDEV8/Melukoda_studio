use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub at: DateTime<Utc>,
    pub subsystem: String,
    pub message: String,
    pub severity: Severity,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
    Error,
}
pub fn redact(value: &str) -> String {
    let mut out = String::new();
    for line in value.lines() {
        let lower = line.to_ascii_lowercase();
        if ["password", "passphrase", "token", "authorization", "secret"]
            .iter()
            .any(|k| lower.contains(k))
        {
            out.push_str("[redacted]\n")
        } else {
            out.push_str(line);
            out.push('\n')
        }
    }
    out
}
pub fn support_bundle(events: &[Event], settings_json: &str) -> String {
    format!(
        "Melukoda Studio support bundle\n\nEvents:\n{}\nSettings (redacted):\n{}",
        serde_json::to_string_pretty(events).unwrap_or_default(),
        redact(settings_json)
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hides_secrets() {
        assert!(!redact("password=hello\nhost=ok").contains("hello"));
    }
}
