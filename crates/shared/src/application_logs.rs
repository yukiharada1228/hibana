//! Bounded, tenant-private application output. Never emit this payload to host logs.
use serde::{Deserialize, Serialize};

pub const MAX_LOG_BYTES: usize = 16 * 1024;
pub const LOG_RETENTION_HOURS: i64 = 24;

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationLogs {
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub truncated: bool,
}

impl std::fmt::Debug for ApplicationLogs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApplicationLogs(<redacted>)")
    }
}

impl ApplicationLogs {
    /// Apply again at the CP trust boundary. JSONB cannot store U+0000; UTF-8
    /// replacement and NUL escaping must not grow past the combined byte budget.
    pub fn bounded(&self) -> Self {
        let mut remaining = MAX_LOG_BYTES;
        let mut truncated = self.truncated;
        let mut sanitize = |input: &str| {
            let mut out = String::new();
            for c in input.chars() {
                let mut bytes = [0; 4];
                let value = if c == '\0' {
                    "\\u0000"
                } else {
                    c.encode_utf8(&mut bytes)
                };
                if value.len() > remaining {
                    truncated = true;
                    break;
                }
                out.push_str(value);
                remaining -= value.len();
            }
            out
        };
        let stdout = sanitize(&self.stdout);
        let stderr = sanitize(&self.stderr);
        Self {
            stdout,
            stderr,
            truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_budget_is_utf8_and_jsonb_safe_and_idempotent() {
        let raw = ApplicationLogs {
            stdout: "\0雪".repeat(MAX_LOG_BYTES),
            stderr: "tail".into(),
            truncated: false,
        };
        let logs = raw.bounded();
        assert!(logs.stdout.len() + logs.stderr.len() <= MAX_LOG_BYTES);
        assert!(!logs.stdout.contains('\0'));
        assert!(logs.truncated);
        assert_eq!(logs.bounded(), logs);
        assert!(!format!("{logs:?}").contains("雪"));
    }
}
