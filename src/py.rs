//! The few Python behaviours CodeLore's texts depend on: `repr()` of a value,
//! `str()` of a float, `round(x, n)`, and `textwrap.fill`.
use serde_json::Value;

/// Python's `repr()` of a string.
pub fn repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            ch if (ch as u32) < 0x20 || ch as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push(quote);
    out
}

/// Python's `repr()` of a JSON value as `json.loads` makes it (None, True,
/// lists and dicts with `', '` and `': '`).
pub fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() => float_str(float),
            _ => number.to_string(),
        },
        Value::String(text) => repr_str(text),
        Value::Array(items) => format!("[{}]", items.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!("{{{}}}", map.iter().map(|(key, value)| format!("{}: {}", repr_str(key), repr(value))).collect::<Vec<_>>().join(", ")),
    }
}

/// `repr()` of an optional value: `None` when it is absent.
pub fn repr_opt(value: Option<&Value>) -> String {
    value.map(repr).unwrap_or_else(|| "None".into())
}

/// Python's `str()` of a float: the shortest form, `.0` on a whole number,
/// an exponent from 1e16 up and below 1e-4.
pub fn float_str(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-4..1e16).contains(&magnitude) {
        // Rust writes 1e-5 and 1.5e16; Python writes 1e-05 and 1.5e+16.
        let text = format!("{value:e}");
        let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
        let exponent: i32 = exponent.parse().unwrap_or(0);
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }
    let text = format!("{value}");
    if text.contains('.') { text } else { format!("{text}.0") }
}

/// Python's `round(value, digits)` for display: correctly rounded from the
/// exact binary value, as Python's is.
pub fn round(value: f64, digits: usize) -> f64 {
    format!("{value:.digits$}").parse().unwrap_or(value)
}

/// `textwrap.fill(text, width, initial_indent, subsequent_indent)`: runs of
/// whitespace become one space and lines break between words. Python also
/// breaks after a hyphen inside a word; this does not.
pub fn fill(text: &str, width: usize, initial: &str, subsequent: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut indent = initial;
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() { word.chars().count() } else { line.chars().count() + 1 + word.chars().count() };
        if !line.is_empty() && indent.chars().count() + candidate > width {
            lines.push(format!("{indent}{line}"));
            indent = subsequent;
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(format!("{indent}{line}"));
    }
    lines.join("\n")
}

/// `textwrap.indent(text, prefix)`: the prefix before every non-blank line.
pub fn indent(text: &str, prefix: &str) -> String {
    text.split_inclusive('\n').map(|line| if line.trim().is_empty() { line.to_owned() } else { format!("{prefix}{line}") }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_reprs_and_floats() {
        assert_eq!(repr(&json!("a'b")), "\"a'b\"");
        assert_eq!(repr(&json!("nope")), "'nope'");
        assert_eq!(repr(&json!({"item": "pl#2"})), "{'item': 'pl#2'}");
        assert_eq!(repr(&json!(["a", 1, null, true])), "['a', 1, None, True]");
        assert_eq!(float_str(1.0), "1.0");
        assert_eq!(float_str(0.667), "0.667");
        assert_eq!(float_str(0.00001), "1e-05");
        assert_eq!(round(2.0 / 3.0, 3), 0.667);
        assert_eq!(fill("one two three four", 12, "  ", "    "), "  one two\n    three\n    four");
    }
}
