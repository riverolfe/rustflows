//! Terminal control characters: C0/C1 (ESC, CR, backspace…) and the bidi
//! overrides that reorder what a line appears to say. Model output reaches the
//! terminal raw, and it is derived from untrusted repos and web pages — an ESC
//! sequence in a "summary" can clear the screen or rewrite earlier lines.

use serde_json::Value;

fn is_ctl(c: char) -> bool {
    let u = c as u32;
    // \t and \n are deliberately absent from the C0 spans.
    (0x00..=0x08).contains(&u)
        || (0x0b..=0x1f).contains(&u)
        || (0x7f..=0x9f).contains(&u)
        || (0x202a..=0x202e).contains(&u)
        || (0x2066..=0x2069).contains(&u)
}

/// Strip control characters; keep `\n` and `\t` unless `newlines` is false.
pub fn clean_text(text: &str, newlines: bool) -> String {
    let cleaned: String = text.chars().filter(|c| !is_ctl(*c)).collect();
    if newlines {
        cleaned
    } else {
        cleaned.replace(['\n', '\t'], " ")
    }
}

/// `clean_text` every string in a JSON-shaped value.
pub fn clean(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(clean_text(s, true)),
        Value::Array(a) => Value::Array(a.iter().map(clean).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), clean(v))).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_esc_and_bidi_but_keeps_newlines() {
        assert_eq!(clean_text("a\x1b[2Jb", true), "a[2Jb");
        assert_eq!(clean_text("a\u{202e}b", true), "ab");
        assert_eq!(clean_text("a\nb\tc", true), "a\nb\tc");
    }

    #[test]
    fn newlines_false_flattens() {
        assert_eq!(clean_text("a\nb\tc", false), "a b c");
    }

    #[test]
    fn recurses_through_json() {
        let got = clean(&json!({"a": ["x\u{7}y", 1, null], "b": {"c": "\u{2066}z"}}));
        assert_eq!(got, json!({"a": ["xy", 1, null], "b": {"c": "z"}}));
    }
}