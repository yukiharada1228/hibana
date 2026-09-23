//! Execution diagnostics must fit both the completion transport and PostgreSQL JSONB.
use std::fmt::{self, Write};

/// Includes the truncation marker. Even worst-case JSON escaping stays below 100 KiB.
pub const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const TRUNCATED: &str = "\n[diagnostic truncated]";

/// Stop formatting at the byte limit; do not first allocate an entire backtrace.
/// NUL is rendered visibly because PostgreSQL JSONB cannot store it.
pub fn format_error(message: impl fmt::Display) -> String {
    let mut writer = DiagnosticWriter {
        text: String::new(),
        truncated: false,
    };
    let _ = write!(writer, "{message}");
    if writer.truncated {
        let end = writer
            .text
            .floor_char_boundary(MAX_DIAGNOSTIC_BYTES - TRUNCATED.len());
        writer.text.truncate(end);
        writer.text.push_str(TRUNCATED);
    }
    writer.text
}

struct DiagnosticWriter {
    text: String,
    truncated: bool,
}

impl DiagnosticWriter {
    fn append(&mut self, text: &str) -> fmt::Result {
        if self.truncated {
            return Err(fmt::Error);
        }
        let end = text.floor_char_boundary(MAX_DIAGNOSTIC_BYTES - self.text.len());
        self.text.push_str(&text[..end]);
        if end < text.len() {
            self.truncated = true;
            return Err(fmt::Error);
        }
        Ok(())
    }
}

impl Write for DiagnosticWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for (index, part) in text.split('\0').enumerate() {
            if index != 0 {
                self.append("\\u0000")?;
            }
            self.append(part)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn preserves_short_diagnostics_and_renders_nul() {
        for (input, expected) in [
            ("", ""),
            ("失敗\n\t原因\r\n", "失敗\n\t原因\r\n"),
            ("\0雪\0\0", "\\u0000雪\\u0000\\u0000"),
            (r"literal \u0000", r"literal \u0000"),
        ] {
            assert_eq!(format_error(input), expected);
        }
        let exact = "a".repeat(MAX_DIAGNOSTIC_BYTES);
        assert_eq!(format_error(&exact), exact);
        assert_eq!(format_error(format_args!("{exact}{}", "")), exact);
    }

    #[test]
    fn truncates_on_utf8_boundaries_after_nul_expansion() {
        for unit in ["x", "雪", "🔥", "\0", "\u{1}"] {
            let input = unit.repeat(MAX_DIAGNOSTIC_BYTES + 1);
            let result = format_error(&input);
            assert!(result.len() <= MAX_DIAGNOSTIC_BYTES);
            assert!(result.ends_with(TRUNCATED));
            assert!(!result.contains('\0'));
            assert!(serde_json::to_vec(&result).unwrap().len() < 100 * 1024);
            assert_eq!(
                format_error(&result),
                result,
                "retries preserve the diagnostic"
            );
        }
        let result = format_error(format_args!("{}雪", "a".repeat(MAX_DIAGNOSTIC_BYTES - 1)));
        assert!(result.ends_with(TRUNCATED));
        assert!(result.len() <= MAX_DIAGNOSTIC_BYTES);
    }

    #[test]
    fn stops_display_before_the_remaining_backtrace_is_formatted() {
        struct Backtrace<'a>(&'a Cell<usize>);
        impl fmt::Display for Backtrace<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for _ in 0..1_000_000 {
                    self.0.set(self.0.get() + 1);
                    f.write_str("frame\n")?;
                }
                panic!("formatter consumed the entire backtrace");
            }
        }
        let writes = Cell::new(0);
        let result = format_error(Backtrace(&writes));
        assert!(result.ends_with(TRUNCATED));
        assert!(result.len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(writes.get() < 3000);
    }
}
