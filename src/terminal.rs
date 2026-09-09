use std::fmt::Write as _;
use std::io::{self, Write};

use anstyle::{AnsiColor, Color, Effects, Style};
use serde_json::Value;

pub const HEADING: Style = Style::new()
    .fg_color(Some(Color::Ansi(AnsiColor::Cyan)))
    .effects(Effects::BOLD);
pub const ACCENT: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan)));
pub const SUCCESS: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
pub const WARNING: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
pub const FAILURE: Style = Style::new()
    .fg_color(Some(Color::Ansi(AnsiColor::Red)))
    .effects(Effects::BOLD);
pub const VALUE: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Magenta)));
pub const MUTED: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::BrightBlack)));
pub const EMPHASIS: Style = Style::new().effects(Effects::BOLD);

pub fn paint(style: Style, value: impl std::fmt::Display) -> String {
    format!("{style}{value}{style:#}")
}

pub fn write_stdout(value: &str) -> io::Result<()> {
    let mut output = anstream::stdout().lock();
    output.write_all(value.as_bytes())?;
    output.flush()
}

pub fn write_stderr(value: &str) -> io::Result<()> {
    let mut output = anstream::stderr().lock();
    output.write_all(value.as_bytes())?;
    output.flush()
}

pub fn pretty_json(value: &Value) -> String {
    let mut output = String::new();
    render_json(value, 0, &mut output);
    output
}

fn render_json(value: &Value, depth: usize, output: &mut String) {
    match value {
        Value::Null => output.push_str(&paint(MUTED, "null")),
        Value::Bool(value) => output.push_str(&paint(WARNING, value)),
        Value::Number(value) => output.push_str(&paint(VALUE, value)),
        Value::String(value) => {
            let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"<invalid>\"".into());
            output.push_str(&paint(SUCCESS, encoded));
        }
        Value::Array(values) if values.is_empty() => output.push_str("[]"),
        Value::Array(values) => {
            output.push_str("[\n");
            for (index, value) in values.iter().enumerate() {
                indent(output, depth + 1);
                render_json(value, depth + 1, output);
                if index + 1 != values.len() {
                    output.push(',');
                }
                output.push('\n');
            }
            indent(output, depth);
            output.push(']');
        }
        Value::Object(values) if values.is_empty() => output.push_str("{}"),
        Value::Object(values) => {
            output.push_str("{\n");
            for (index, (key, value)) in values.iter().enumerate() {
                indent(output, depth + 1);
                let encoded = serde_json::to_string(key).unwrap_or_else(|_| "\"<invalid>\"".into());
                output.push_str(&paint(ACCENT, encoded));
                output.push_str(": ");
                render_json(value, depth + 1, output);
                if index + 1 != values.len() {
                    output.push(',');
                }
                output.push('\n');
            }
            indent(output, depth);
            output.push('}');
        }
    }
}

fn indent(output: &mut String, depth: usize) {
    let _ = write!(output, "{:width$}", "", width = depth * 2);
}

pub fn strip_ansi(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        let Some(kind) = bytes.get(index).copied() else {
            break;
        };
        index += 1;
        match kind {
            b'[' => {
                while let Some(byte) = bytes.get(index).copied() {
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            b']' => {
                while let Some(byte) = bytes.get(index).copied() {
                    index += 1;
                    if byte == 0x07 {
                        break;
                    }
                    if byte == 0x1b && bytes.get(index) == Some(&b'\\') {
                        index += 1;
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn styled_json_preserves_the_plain_pretty_document() {
        let value = json!({"string":"value","number":7,"boolean":true,"null":null,"array":[1,2]});
        let styled = pretty_json(&value);
        assert!(styled.contains("\u{1b}["));
        assert_eq!(
            strip_ansi(&styled),
            serde_json::to_string_pretty(&value).unwrap()
        );
    }

    #[test]
    fn ansi_stripping_handles_csi_osc_and_unicode() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m café"), "red café");
        assert_eq!(strip_ansi("before\u{1b}]0;title\u{7}after"), "beforeafter");
        assert_eq!(
            strip_ansi("before\u{1b}]0;title\u{1b}\\after"),
            "beforeafter"
        );
    }
}
