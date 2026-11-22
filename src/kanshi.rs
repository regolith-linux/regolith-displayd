//! Writing monitor descriptions into kanshi profiles, whatever make, model
//! and serial number a monitor reports, for every kanshi version.

use std::fmt::Write;

/// Whether every kanshi version reads `description`, written as an output
/// name between double quotes, as exactly that description, and matches no
/// other monitor with it. That is not so if it holds:
///
/// - a double quote: kanshi before 1.6 ends the name there, and kanshi 1.6
///   and later need it escaped with a backslash, which kanshi before 1.6
///   reads as it is;
/// - a backslash, for the same reason;
/// - a control character: kanshi 1.6 and later refuse a newline in a quoted
///   string;
/// - `*`, `?` or `[`: kanshi 1.8 and later match the name as an fnmatch(3)
///   pattern, so it would match other monitors too;
/// - a `$` first: kanshi 1.7 and later take the name for an output alias.
///
/// A monitor whose description is not read back as it is gets named by its
/// connector instead.
pub fn is_verbatim_output_name(description: &str) -> bool {
    !description.starts_with('$')
        && !description
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\\' | '*' | '?' | '['))
}

/// `text` between double quotes, for the comments in which this daemon
/// records what a profile was saved for: backslashes and double quotes are
/// escaped with a backslash, and control characters are written as
/// `\u{...}`, so the comment stays on one line. Text without any of those is
/// quoted as it is, which is how earlier versions wrote every description.
/// [`unquote`] reads it back.
pub fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        match c {
            '\\' | '"' => {
                quoted.push('\\');
                quoted.push(c);
            }
            c if c.is_control() => write!(quoted, "\\u{{{:x}}}", c as u32).unwrap(),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// The text that [`quote`] wrote at the start of `quoted`, and what follows
/// it; None if `quoted` does not start with such a string.
pub fn unquote(quoted: &str) -> Option<(String, &str)> {
    let body = quoted.strip_prefix('"')?;
    let mut text = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((text, &body[i + 1..])),
            '\\' => match chars.next()?.1 {
                c @ ('\\' | '"') => text.push(c),
                'u' => {
                    let rest = chars.as_str().strip_prefix('{')?;
                    let (code, _) = rest.split_once('}')?;
                    if code.is_empty() || !code.chars().all(|c| c.is_ascii_hexdigit()) {
                        return None;
                    }
                    text.push(char::from_u32(u32::from_str_radix(code, 16).ok()?)?);
                    // Past "{", the digits and "}".
                    chars.nth(code.len() + 1)?;
                }
                _ => return None,
            },
            c if c.is_control() => return None,
            c => text.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_text_reads_back() {
        for text in [
            "Acme View 24 0",
            "Acme \"Pro\" M\\1 S\\",
            "Acme\"\n}\nprofile {\n\toutput \"*\" disable",
            "X\u{1}\t\r\u{7f}\u{85}\\n\" # tail",
            "Écran 显示器 S1",
            "",
        ] {
            let quoted = quote(text);
            assert!(!quoted.chars().any(char::is_control), "{quoted}");
            assert_eq!(unquote(&quoted), Some((text.to_string(), "")));
            assert_eq!(
                unquote(&format!("{quoted} is \"x\"")),
                Some((text.to_string(), " is \"x\""))
            );
        }
        // As earlier versions wrote it.
        assert_eq!(quote("Acme View 24 0"), "\"Acme View 24 0\"");
        assert_eq!(quote("A\nB"), "\"A\\u{a}B\"");
        for invalid in [
            "\"A\\x\"",
            "\"unterminated",
            "\"A\\u{110000}\"",
            "\"A\\u{a\"",
            "\"A\\u{+41}\"",
            "\"A\\u{}\"",
            "A",
        ] {
            assert_eq!(unquote(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn only_descriptions_every_kanshi_reads_back_are_verbatim() {
        for verbatim in [
            "Acme View 24 0",
            "Dell Inc. DELL U2415 7MT0167G1V0L",
            "Acme $x S1",
            "../Acme M/1 S1",
            "Écran {1} #2 'x' ] S1",
            "  ",
        ] {
            assert!(is_verbatim_output_name(verbatim), "{verbatim}");
        }
        for named_by_connector in [
            "Acme \"Pro\" M1 S1",
            "Acme M\\1 S1",
            "Acme\n} M1 S1",
            "Acme M\t1 S1",
            "Acme View* S1",
            "Acme M? S1",
            "Acme [x] S1",
            "$work M1 S1",
        ] {
            assert!(
                !is_verbatim_output_name(named_by_connector),
                "{named_by_connector}"
            );
        }
    }
}
