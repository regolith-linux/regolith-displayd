//! Writing monitor descriptions into kanshi profiles, and into their file
//! names, whatever make, model and serial number a monitor reports, for
//! every kanshi version; and asking kanshi whether it loads a configuration.

use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fmt::Write;
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long [`loads`] waits for kanshi to read a configuration.
const LOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// A Wayland display that cannot be connected to: an absolute path, which
/// libwayland connects to as it is, under a file that is not a directory.
const NO_DISPLAY: &str = "/dev/null/regolith-displayd-config-check";

/// Whether the `kanshi` executable loads the configuration file `config`,
/// with everything it includes; None if that cannot be told, because kanshi
/// cannot be run, does not finish in time (an include may name a FIFO) or
/// is killed by a signal.
///
/// kanshi 1.2 and later read the configuration given with `-c`, includes
/// and all, before they connect to the compositor, and stop with "failed to
/// connect to display" if they cannot connect. So kanshi is run where it
/// cannot connect, and does nothing but read the configuration: without
/// `WAYLAND_SOCKET`, and with a `WAYLAND_DISPLAY` that does not exist. The
/// configuration loads if kanshi gets that far. This asks the installed
/// kanshi itself, whose syntax depends on its version. kanshi 1.1 ignores
/// `-c` (see [`can_tell`]).
pub fn loads(kanshi: &OsStr, config: &Path) -> Option<bool> {
    let mut child = Command::new(kanshi)
        .arg("-c")
        .arg(config)
        .env_remove("WAYLAND_SOCKET")
        .env("WAYLAND_DISPLAY", NO_DISPLAY)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stderr = child.stderr.take()?;
    let reader = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.read_to_end(&mut text);
        text
    });
    let deadline = Instant::now() + LOAD_TIMEOUT;
    let exited = loop {
        match child.try_wait() {
            // Killed, as by `pkill kanshi`: it did not get to say.
            Ok(Some(status)) => break status.signal().is_none(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let stderr = reader.join().ok()?;
    exited.then(|| String::from_utf8_lossy(&stderr).contains("failed to connect to display"))
}

/// Whether [`loads`] can tell anything with the `kanshi` executable: it
/// loads an empty configuration. kanshi 1.1 reads its default configuration
/// whatever it is given, and a program that never says "failed to connect
/// to display" is not kanshi as [`loads`] knows it.
pub fn can_tell(kanshi: &OsStr) -> bool {
    loads(kanshi, Path::new("/dev/null")) == Some(true)
}

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

/// The longest file name, in bytes.
pub const NAME_MAX: usize = 255;

/// A monitor as a profile file name names it: its connector, if the profile
/// names it by its connector, and its description.
pub type NamedOutput = (Option<String>, String);

/// The file name of the kanshi profile for the monitors `outputs`.
///
/// It joins the description of each monitor, or "connector=description" for
/// a monitor named by its connector, sorted, with "__" between them and
/// spaces as "_", after "zz-" if any monitor is named by its connector. If
/// every description and connector is plain (see [`is_plain`]), that is the
/// name earlier versions gave the profile (see [`earlier_profile_file_name`]).
/// Otherwise each of them is escaped first (see [`file_name_part`]). Either
/// way the name is one path component, does not start with ".", and differs
/// for any two sets of monitors. A name longer than a file name can be is
/// shortened (see [`shorten`]).
pub fn profile_file_name(outputs: &[NamedOutput]) -> String {
    let plain = outputs.iter().all(|(connector, description)| {
        is_plain(description)
            && connector.as_deref().map_or(true, |connector| {
                is_plain(connector) && !connector.contains(' ')
            })
    });
    shorten(join(outputs, !plain), outputs)
}

/// The file name that earlier versions gave the profile for `outputs`, as
/// they named the monitors: their names joined as they are, which may not be
/// a file name at all.
pub fn earlier_profile_file_name(outputs: &[NamedOutput]) -> String {
    join(outputs, false)
}

/// Whether profile file names hold `text` as it is: ASCII letters and
/// digits, spaces and ".-,+()" only, no two spaces in a row, no space and
/// no "." first. A make or model that is empty, or a make, model or serial
/// number that starts or ends with a space, makes the description not plain.
///
/// An empty serial number alone leaves the description plain, though it
/// then ends with a space: sway before 1.8 reports it so for monitors
/// without one (every headless output, for one), and earlier versions named
/// their profiles "Make_Model_". Names stay distinct: in the joined name a
/// run of one "_" is a space, of two the "__" between monitors, of three a
/// space that ends a monitor's name and then "__", and a "_" at the very end
/// is a space that ends the last name, since no name starts with a space or
/// holds two in a row.
fn is_plain(text: &str) -> bool {
    !text.starts_with(['.', ' '])
        && !text.contains("  ")
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " .-,+()".contains(c))
}

/// `text` in the file name of a profile for monitors that are not all
/// plain: ASCII letters, digits and ".-,+()" as they are, a space as "_",
/// and anything else, "_", "%" and "=" included, as "%XX" for each of its
/// UTF-8 bytes. A space that starts or ends the text or follows another
/// one, and a "." that starts it, are escaped too. So the part never starts
/// with ".", and holds neither "__" nor "_" at either end, which keeps the
/// "__" between monitors and the "=" after a connector apart from it. A
/// text that is not plain comes out with a "%", or, for a connector that
/// holds a space, with a "_" before the "=": the name of a profile for plain
/// monitors has neither.
fn file_name_part(text: &str) -> String {
    let mut part = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut previous = None;
    while let Some(c) = chars.next() {
        match c {
            ' ' if previous.is_some_and(|p| p != ' ') && chars.peek().is_some() => part.push('_'),
            '.' if previous.is_some() => part.push('.'),
            c if c.is_ascii_alphanumeric() || "-,+()".contains(c) => part.push(c),
            c => {
                let mut buf = [0; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    write!(part, "%{byte:02X}").unwrap();
                }
            }
        }
        previous = Some(c);
    }
    part
}

/// The profile file name for `outputs`: the names of the monitors, sorted
/// and joined with "__", after "zz-" if any monitor is named by its
/// connector. As earlier versions wrote them, with spaces as "_", or
/// `escaped` with [`file_name_part`].
fn join(outputs: &[NamedOutput], escaped: bool) -> String {
    let mut names: Vec<String> = outputs
        .iter()
        .map(|(connector, description)| match (connector, escaped) {
            (Some(connector), false) => format!("{connector}={description}"),
            (None, false) => description.clone(),
            (Some(connector), true) => format!(
                "{}={}",
                file_name_part(connector),
                file_name_part(description)
            ),
            (None, true) => file_name_part(description),
        })
        .collect();
    names.sort();
    let mut name = names.join("__").replace(' ', "_");
    if outputs.iter().any(|(connector, _)| connector.is_some()) {
        name.insert_str(0, "zz-");
    }
    name
}

/// `name` if a file can have that name. Else its start, then "~" and the
/// SHA-256 digest of the connectors and descriptions of all the monitors it
/// is for, in [`NAME_MAX`] bytes: the digest tells sets of monitors apart,
/// and names that need no shortening never hold a "~".
fn shorten(name: String, outputs: &[NamedOutput]) -> String {
    if name.len() <= NAME_MAX {
        return name;
    }
    let mut sorted: Vec<&NamedOutput> = outputs.iter().collect();
    sorted.sort();
    let mut hasher = Sha256::new();
    for (connector, description) in sorted {
        // Each text after its length, so that no two sets read the same.
        if let Some(connector) = connector {
            hasher.update(b"c");
            hasher.update((connector.len() as u64).to_be_bytes());
            hasher.update(connector);
        }
        hasher.update(b"d");
        hasher.update((description.len() as u64).to_be_bytes());
        hasher.update(description);
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // Plain and escaped names alike are ASCII: any byte ends a character.
    debug_assert!(name.is_ascii());
    format!("{}~{digest}", &name[..NAME_MAX - 1 - digest.len()])
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

    fn by_description(descriptions: &[&str]) -> Vec<NamedOutput> {
        descriptions
            .iter()
            .map(|description| (None, description.to_string()))
            .collect()
    }

    fn by_connector(outputs: &[(&str, &str)]) -> Vec<NamedOutput> {
        outputs
            .iter()
            .map(|(connector, description)| (Some(connector.to_string()), description.to_string()))
            .collect()
    }

    #[test]
    fn plain_sets_keep_the_file_names_of_earlier_versions() {
        for outputs in [
            by_description(&["Acme View 24 0"]),
            by_description(&[
                "LG 27GL850 007NTAB1C123",
                "Dell Inc. DELL U2415 7MT0167G1V0L",
            ]),
            by_connector(&[("DP-1", "Acme View 24 0"), ("DP-2", "Acme View 24 0")]),
            by_description(&["Lenovo Group Limited 0x40BA (1,2+3) Unknown"]),
            // An empty serial number, as sway before 1.8 reports it.
            by_description(&["headless headless "]),
            by_description(&["Acme View ", "BOE 0x0BCA Unknown"]),
            by_connector(&[
                ("HEADLESS-1", "headless headless "),
                ("HEADLESS-2", "headless headless "),
            ]),
        ] {
            assert_eq!(
                profile_file_name(&outputs),
                earlier_profile_file_name(&outputs)
            );
        }
        assert_eq!(
            profile_file_name(&by_description(&["Acme View 24 0", "BOE 0x0BCA Unknown"])),
            "Acme_View_24_0__BOE_0x0BCA_Unknown"
        );
        assert_eq!(
            profile_file_name(&by_connector(&[
                ("DP-2", "Acme View 24 0"),
                ("DP-1", "Acme View 24 0")
            ])),
            "zz-DP-1=Acme_View_24_0__DP-2=Acme_View_24_0"
        );
        assert_eq!(
            profile_file_name(&by_connector(&[
                ("HEADLESS-2", "headless headless "),
                ("HEADLESS-1", "headless headless ")
            ])),
            "zz-HEADLESS-1=headless_headless___HEADLESS-2=headless_headless_"
        );
    }

    #[test]
    fn profile_file_names_tell_monitor_sets_apart() {
        let sets = [
            by_description(&["Acme View 24 0"]),
            by_description(&["Acme View_24 0"]),
            by_description(&["Acme View%5F24 0"]),
            by_description(&["A B C", "D E F"]),
            by_description(&["A B C  D E F"]),
            by_description(&["A B C__D E F"]),
            by_description(&["A B C D E F"]),
            by_connector(&[("DP-1", "Acme View 24 0"), ("DP-2", "Acme View 24 0")]),
            by_description(&["zz-DP-1=Acme View 24 0__DP-2=Acme View 24 0"]),
            by_connector(&[("DP-1", "Acme M\n1 S1"), ("DP-2", "Acme M\r2 S2")]),
            by_connector(&[("DP-1", "Acme M\r2 S2"), ("DP-2", "Acme M\n1 S1")]),
            by_description(&["  "]),
            by_description(&[".hidden M1 S1"]),
            by_description(&[" Acme View S1 "]),
            by_description(&["Acme  View S1 "]),
            by_description(&["Acme View  S1"]),
            by_description(&["Acme View S1 "]),
            by_description(&["../Acme M/1 S1"]),
            by_description(&["Écran 显示器 S1"]),
            by_description(&["Acme ~ S1"]),
            by_description(&["A B ", "C D"]),
            by_description(&["A B", "C D "]),
            by_description(&["A B ", "C D "]),
            by_description(&["A B", " C D"]),
            by_description(&["A B   C D"]),
            by_description(&["A B ", " C D"]),
            by_description(&["A B C "]),
            by_description(&["A B C"]),
            by_connector(&[("DP-1", "A B "), ("DP-2", "A B ")]),
            by_connector(&[("DP-1", "A B"), ("DP-2", "A B")]),
        ];
        let names: Vec<String> = sets.iter().map(|set| profile_file_name(set)).collect();
        let distinct: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(distinct.len(), names.len(), "{names:#?}");
        for name in &names {
            assert!(
                !name.contains('/')
                    && !name.starts_with('.')
                    && !name.is_empty()
                    && name.len() <= NAME_MAX,
                "{name}"
            );
        }
        assert_eq!(names[1], "Acme_View%5F24_0");
        assert_eq!(names[13], "%20Acme_View_S1%20");
        assert_eq!(names[16], "Acme_View_S1_");
        assert_eq!(names[20], "A_B___C_D");
        assert_eq!(names[21], "A_B__C_D_");
        assert_eq!(names[17], "%2E.%2FAcme_M%2F1_S1");
        assert_eq!(names[18], "%C3%89cran_%E6%98%BE%E7%A4%BA%E5%99%A8_S1");
        // Monitors named by their descriptions can swap connectors.
        assert_eq!(
            names[3],
            profile_file_name(&by_description(&["D E F", "A B C"]))
        );
    }

    #[test]
    fn long_profile_file_names_are_shortened() {
        let long = |serial: &str| {
            profile_file_name(&by_description(&[
                "Acme View 24 0",
                &format!("Zeta {} {serial}", "L".repeat(300)),
            ]))
        };
        let x = long("SerialX");
        assert_ne!(x, long("SerialY"));
        assert_eq!(x, long("SerialX"));
        assert_eq!(x.len(), NAME_MAX);
        assert!(x.starts_with("Acme_View_24_0__Zeta_LLL"), "{x}");
        assert_eq!(
            &x[x.len() - 65..],
            "~ae8b665a016166ab8585e6c7f75d99841132859d4e942fb22acc3c46c21fcf63"
        );
        // A connector, or another one, makes another set.
        let description = format!("Acme {} S1", "M".repeat(300));
        let names = [
            profile_file_name(&by_description(&[&description])),
            profile_file_name(&by_connector(&[("DP-1", &description)])),
            profile_file_name(&by_connector(&[("DP-2", &description)])),
            profile_file_name(&by_connector(&[("DP-1", &format!("{description}\n"))])),
        ];
        for (i, name) in names.iter().enumerate() {
            assert!(name.len() <= NAME_MAX && name.contains('~'), "{name}");
            assert!(!names[..i].contains(name), "{name}");
        }
        assert!(profile_file_name(&by_description(&[&"显".repeat(100)])).len() <= NAME_MAX);
    }
}
