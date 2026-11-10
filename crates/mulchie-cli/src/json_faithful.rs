//! Whether rewriting a JSON file through `serde_json::Value` would change
//! what it says. Install commands edit other programs' settings files by
//! parsing and re-serialising them, which loses two things silently: numbers
//! beyond what an `f64` or a 64-bit integer holds (`12345678901234567890123`
//! comes back as `1.2345678901234568e+22`), and all but the last of a
//! duplicated key. A file with either is refused and left as it is.

use std::collections::HashSet;

/// `Ok` when every number survives the round trip and no object repeats a
/// key; otherwise why not. The text must already parse as JSON.
pub fn check(text: &str) -> Result<(), String> {
    enum Frame {
        Obj {
            keys: HashSet<String>,
            want_key: bool,
        },
        Arr,
    }
    let b = text.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => {
                stack.push(Frame::Obj {
                    keys: HashSet::new(),
                    want_key: true,
                });
                i += 1;
            }
            b'[' => {
                stack.push(Frame::Arr);
                i += 1;
            }
            b'}' | b']' => {
                stack.pop();
                i += 1;
            }
            b',' => {
                if let Some(Frame::Obj { want_key, .. }) = stack.last_mut() {
                    *want_key = true;
                }
                i += 1;
            }
            b':' => {
                if let Some(Frame::Obj { want_key, .. }) = stack.last_mut() {
                    *want_key = false;
                }
                i += 1;
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
                let raw = &text[start..i.min(b.len())];
                if let Some(Frame::Obj {
                    keys,
                    want_key: true,
                }) = stack.last_mut()
                {
                    let key: String = serde_json::from_str(raw).unwrap_or_else(|_| raw.to_string());
                    if !keys.insert(key.clone()) {
                        return Err(format!("the key {key:?} appears twice in one object"));
                    }
                }
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < b.len() && matches!(b[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    i += 1;
                }
                let tok = &text[start..i];
                if !number_survives(tok) {
                    return Err(format!(
                        "the number {tok} cannot be rewritten exactly (it would come back as {})",
                        serde_json::from_str::<serde_json::Value>(tok)
                            .map(|v| v.to_string())
                            .unwrap_or_else(|_| "something else".into())
                    ));
                }
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// Whether a JSON number token keeps its value through `serde_json`.
fn number_survives(tok: &str) -> bool {
    let integer = !tok.contains(['.', 'e', 'E']);
    if integer {
        return tok.parse::<i64>().is_ok() || tok.parse::<u64>().is_ok();
    }
    // An f64 holds 15 significant decimal digits exactly; more may change.
    let mantissa = tok.split(['e', 'E']).next().unwrap_or("");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let significant = digits.trim_start_matches('0').trim_end_matches('0');
    significant.len() <= 15 && tok.parse::<f64>().is_ok_and(f64::is_finite)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossy_numbers_and_duplicate_keys_are_found() {
        assert!(check(r#"{"a": 1, "b": [1.5, -2, 3e2, 0.000125], "c": {"a": "x\"y"}, "d": 18446744073709551615}"#).is_ok());
        let e = check(r#"{"n": 12345678901234567890123}"#).unwrap_err();
        assert!(
            e.contains("12345678901234567890123") && e.contains("e+22"),
            "{e}"
        );
        assert!(check(r#"{"n": 0.12345678901234567890}"#).is_err());
        let e = check(r#"{"hooks": {}, "x": 1, "hooks": {"a": 1}}"#).unwrap_err();
        assert!(e.contains("\"hooks\" appears twice"), "{e}");
        // The same key in different objects, and keys inside strings, are fine.
        assert!(check(r#"{"a": {"k": 1}, "b": {"k": 2}, "s": "{\"k\":1,\"k\":2}"}"#).is_ok());
        // Values that are strings equal to keys are not keys.
        assert!(check(r#"{"a": "a", "b": ["a", "a"]}"#).is_ok());
    }
}
