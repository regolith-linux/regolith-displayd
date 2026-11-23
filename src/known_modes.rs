//! The modes to offer for a monitor that is off when sway reports none.
//!
//! sway reports no modes for a headless or other virtual output that is
//! off. While the daemon runs, [`Monitor::keep_known_modes`] offers the
//! modes such a monitor had when it was on, but one that is already off
//! when the daemon starts would have none, and the Displays panel could
//! never switch it back on. It is offered instead, from the first of these
//! that has one:
//!
//! 1. the [`ModeHistory`]: the mode the same monitor was last in while it
//!    was on, kept in a file that survives restarts;
//! 2. the modes the saved kanshi profiles set for the monitor (see
//!    [`profile_modes`]), for a monitor never seen on since the history
//!    was started;
//! 3. [`FALLBACK_MODE`].
//!
//! [`Monitor::keep_known_modes`]: crate::monitor::Monitor::keep_known_modes

use crate::modes::ModeKey;
use crate::monitor::MonitorId;
use log::warn;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The mode offered when nothing else names one: the size sway gives an
/// output made with `create_output`, at whatever refresh rate sway picks.
pub const FALLBACK_MODE: ModeKey = (1920, 1080, 0);

/// The mode each monitor was in the last time it was on, kept in a file so
/// that it survives restarts of the daemon and of sway.
pub struct ModeHistory {
    /// Where the history is saved, if there is somewhere to save it.
    path: Option<PathBuf>,
    modes: BTreeMap<MonitorId, ModeKey>,
}

impl ModeHistory {
    /// The history saved at `path`. A missing or unreadable file is an
    /// empty history, and lines that don't parse are skipped.
    pub fn load(path: Option<PathBuf>) -> ModeHistory {
        let modes = match path.as_deref().map(fs::read_to_string) {
            Some(Ok(contents)) => parse_history(&contents),
            Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => {
                warn!("Cannot read the mode history: {e}");
                BTreeMap::new()
            }
            _ => BTreeMap::new(),
        };
        ModeHistory { path, modes }
    }

    /// The mode `monitor` was last in while it was on.
    pub fn mode(&self, monitor: &MonitorId) -> Option<ModeKey> {
        self.modes.get(monitor).copied()
    }

    /// Remember that `monitor` is on in `mode`, and save the history if that
    /// is new. A history that cannot be saved still holds for this run.
    pub fn remember(&mut self, monitor: &MonitorId, mode: ModeKey) {
        if self.mode(monitor) == Some(mode) {
            return;
        }
        self.modes.insert(monitor.clone(), mode);
        if let Some(path) = &self.path {
            if let Err(e) = write_atomically(path, format_history(&self.modes).as_bytes()) {
                warn!("Cannot save the mode history to {}: {e}", path.display());
            }
        }
    }
}

/// Where the mode history is kept: `regolith-displayd/modes` in
/// `$XDG_STATE_HOME`, or in `~/.local/state` when that is not set.
pub fn history_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| Some(crate::home_dir()?.join(".local").join("state")))?;
    Some(state.join("regolith-displayd").join("modes"))
}

/// A mode as the history writes it: `WIDTHxHEIGHT@MILLIHERTZ`.
fn format_mode((width, height, refresh): ModeKey) -> String {
    format!("{width}x{height}@{refresh}")
}

fn parse_mode(text: &str) -> Option<ModeKey> {
    let (size, refresh) = text.split_once('@')?;
    let (width, height) = size.split_once('x')?;
    let mode = (
        width.parse().ok()?,
        height.parse().ok()?,
        refresh.parse().ok()?,
    );
    (mode.0 > 0 && mode.1 > 0 && mode.2 >= 0).then_some(mode)
}

/// Each monitor on a line: its connector, make, model and serial, then its
/// mode, separated by tabs. A monitor whose names hold a tab or a line
/// break is left out.
fn format_history(history: &BTreeMap<MonitorId, ModeKey>) -> String {
    let mut text = String::new();
    for ((connector, make, model, serial), &mode) in history {
        let fields = [connector, make, model, serial];
        if fields
            .iter()
            .any(|field| field.contains(['\t', '\n', '\r']))
        {
            continue;
        }
        for field in fields {
            text.push_str(field);
            text.push('\t');
        }
        text.push_str(&format_mode(mode));
        text.push('\n');
    }
    text
}

fn parse_history(contents: &str) -> BTreeMap<MonitorId, ModeKey> {
    let mut history = BTreeMap::new();
    for line in contents.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let [connector, make, model, serial, mode] = fields.as_slice() else {
            continue;
        };
        if let Some(mode) = parse_mode(mode) {
            let monitor = (
                connector.to_string(),
                make.to_string(),
                model.to_string(),
                serial.to_string(),
            );
            history.insert(monitor, mode);
        }
    }
    history
}

/// Replace `path` with `contents` in one step, so that a daemon killed
/// mid-write leaves the previous file whole.
fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("tmp");
    let written = fs::File::create(&temporary)
        .and_then(|mut file| {
            file.write_all(contents)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temporary, path));
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// The words of a line of kanshi configuration, as kanshi reads them:
/// separated by spaces or tabs, a word with spaces in double or single
/// quotes, and a backslash outside single quotes taking the next character
/// as it is. No words for a comment, and None when a quote isn't closed.
fn words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|&c| c == ' ' || c == '\t').is_some() {}
        match chars.peek() {
            None => return Some(words),
            Some('#') if words.is_empty() => return Some(words),
            Some(_) => {}
        }
        let mut word = String::new();
        let mut quote = None;
        while let Some(c) = chars.next() {
            match (quote, c) {
                (None, ' ' | '\t') => break,
                (None, '"' | '\'') => quote = Some(c),
                (Some(open), _) if c == open => quote = None,
                (Some('\''), _) => word.push(c),
                (_, '\\') => word.push(chars.next()?),
                _ => word.push(c),
            }
        }
        if quote.is_some() {
            return None;
        }
        words.push(word);
    }
}

/// A kanshi mode: `WIDTHxHEIGHT`, optionally followed by `@RATE` and
/// `Hz`, the rate in hertz with or without decimals.
fn kanshi_mode(text: &str) -> Option<ModeKey> {
    let (size, rate) = match text.split_once('@') {
        Some((size, rate)) => (size, rate.strip_suffix("Hz").unwrap_or(rate)),
        None => (text, "0"),
    };
    let (width, height) = size.split_once('x')?;
    let (width, height): (i32, i32) = (width.parse().ok()?, height.parse().ok()?);
    let rate: f64 = rate.parse().ok()?;
    if width <= 0 || height <= 0 || !(0.0..1e6).contains(&rate) {
        return None;
    }
    Some((width, height, (rate * 1000.0).round() as i32))
}

/// The mode an `output` line of a kanshi profile sets for an output named
/// one of `names`: the last `mode`, as in kanshi.
fn output_mode(line: &str, names: &[&str]) -> Option<ModeKey> {
    let words = words(line)?;
    let [directive, name, params @ ..] = words.as_slice() else {
        return None;
    };
    if directive != "output" || !names.contains(&name.as_str()) {
        return None;
    }
    let mut params = params.iter();
    let mut mode = None;
    while let Some(param) = params.next() {
        if param != "mode" {
            continue;
        }
        let mut value = params.next();
        if value.is_some_and(|value| value == "--custom") {
            value = params.next();
        }
        if let Some(parsed) = value.and_then(|value| kanshi_mode(value)) {
            mode = Some(parsed);
        }
    }
    mode
}

/// The modes the kanshi profiles in `profiles` set for the monitor with
/// `description` ("make model serial") on `connector`, file by file in name
/// order and line by line, without repeats. Like the kanshi configuration's
/// `include` of the directory, this skips hidden files. An output named by
/// its connector counts unless the profile records, in the comment this
/// daemon writes for it, that it was saved for another monitor on that
/// connector.
pub fn profile_modes(profiles: &Path, description: &str, connector: &str) -> Vec<ModeKey> {
    let mut files: Vec<PathBuf> = fs::read_dir(profiles)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    let mut modes = Vec::new();
    for file in files {
        let Ok(profile) = fs::read_to_string(&file) else {
            continue;
        };
        let saved_for_another = crate::pinned_outputs(&profile)
            .iter()
            .any(|(pinned, saved)| pinned == connector && saved != description);
        // Bound first, so the slice outlives the `if` on rustc before 1.79.
        let both = [description, connector];
        let names: &[&str] = if saved_for_another { &both[..1] } else { &both };
        for mode in profile.lines().filter_map(|line| output_mode(line, names)) {
            if !modes.contains(&mode) {
                modes.push(mode);
            }
        }
    }
    modes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headless(connector: &str) -> MonitorId {
        (
            connector.to_string(),
            "Unknown".to_string(),
            "Unknown".to_string(),
            "Unknown".to_string(),
        )
    }

    /// An empty directory of its own for each test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "displayd-known-modes-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_history_survives_a_restart() {
        let dir = scratch("history");
        let path = dir.join("state").join("modes");
        let mut history = ModeHistory::load(Some(path.clone()));
        assert_eq!(history.mode(&headless("HEADLESS-1")), None);
        history.remember(&headless("HEADLESS-1"), (1600, 1000, 0));
        history.remember(&headless("HEADLESS-2"), (1280, 720, 59940));
        // A later mode replaces the earlier one.
        history.remember(&headless("HEADLESS-1"), (1280, 800, 0));

        let restarted = ModeHistory::load(Some(path.clone()));
        assert_eq!(
            restarted.mode(&headless("HEADLESS-1")),
            Some((1280, 800, 0))
        );
        assert_eq!(
            restarted.mode(&headless("HEADLESS-2")),
            Some((1280, 720, 59940))
        );
        // Same connector, another monitor.
        let other = (
            "HEADLESS-1".to_string(),
            "Acme".to_string(),
            "Unknown".to_string(),
            "Unknown".to_string(),
        );
        assert_eq!(restarted.mode(&other), None);
        assert!(!path.with_extension("tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_history_that_cannot_be_saved_holds_for_the_run() {
        let dir = scratch("unwritable");
        let blocker = dir.join("not-a-directory");
        fs::write(&blocker, "").unwrap();
        let mut history = ModeHistory::load(Some(blocker.join("modes")));
        history.remember(&headless("HEADLESS-1"), (1600, 1000, 0));
        assert_eq!(history.mode(&headless("HEADLESS-1")), Some((1600, 1000, 0)));
        let mut nowhere = ModeHistory::load(None);
        nowhere.remember(&headless("HEADLESS-1"), (1600, 1000, 0));
        assert_eq!(nowhere.mode(&headless("HEADLESS-1")), Some((1600, 1000, 0)));
        // Names the file cannot hold are remembered, but not saved.
        let mut odd = headless("HEADLESS-1");
        odd.3 = "A\tB".to_string();
        let path = dir.join("modes");
        let mut saved = ModeHistory::load(Some(path.clone()));
        saved.remember(&odd, (800, 600, 0));
        saved.remember(&headless("HEADLESS-2"), (1024, 768, 0));
        assert_eq!(saved.mode(&odd), Some((800, 600, 0)));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "HEADLESS-2\tUnknown\tUnknown\tUnknown\t1024x768@0\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn broken_history_lines_are_skipped() {
        let history = parse_history(
            "junk\n\
             HEADLESS-1\tUnknown\tUnknown\tUnknown\n\
             HEADLESS-2\tUnknown\tUnknown\tUnknown\t0x0@0\n\
             HEADLESS-3\tUnknown\tUnknown\tUnknown\tWxH@60\n\
             HEADLESS-4\tUnknown\tUnknown\tUnknown\t800x600@60000\textra\n\
             HEADLESS-5\tUnknown\tUnknown\tUnknown\t800x600@60000\n",
        );
        assert_eq!(history.len(), 1);
        assert_eq!(history[&headless("HEADLESS-5")], (800, 600, 60000));
        let text = format_history(&history);
        assert_eq!(
            text,
            "HEADLESS-5\tUnknown\tUnknown\tUnknown\t800x600@60000\n"
        );
        assert_eq!(parse_history(&text), history);
    }

    #[test]
    fn kanshi_lines_are_split_into_words_as_kanshi_does() {
        let split = |line| words(line).unwrap();
        assert_eq!(
            split("\toutput \"Unknown Unknown Unknown\" mode 1600x1000 enable"),
            [
                "output",
                "Unknown Unknown Unknown",
                "mode",
                "1600x1000",
                "enable"
            ]
        );
        assert_eq!(
            split("output 'A \\b' Unknown\\ Unknown \"a\\\"b\""),
            ["output", "A \\b", "Unknown Unknown", "a\"b"]
        );
        assert!(split("  # output HEADLESS-1 mode 800x600").is_empty());
        assert_eq!(words("output \"HEADLESS-1 mode 800x600"), None);
        assert_eq!(words("output HEADLESS-1 \\"), None);
    }

    #[test]
    fn kanshi_modes_parse_with_and_without_a_rate() {
        assert_eq!(kanshi_mode("1600x1000"), Some((1600, 1000, 0)));
        assert_eq!(kanshi_mode("1280x720@60Hz"), Some((1280, 720, 60000)));
        assert_eq!(kanshi_mode("1920x1080@59.951"), Some((1920, 1080, 59951)));
        assert_eq!(kanshi_mode("1920x1080@59.94Hz"), Some((1920, 1080, 59940)));
        for broken in [
            "",
            "preferred",
            "1920x",
            "0x1080",
            "1920x1080@",
            "1x1@-1",
            "1x1@inf",
        ] {
            assert_eq!(kanshi_mode(broken), None, "{broken}");
        }
    }

    #[test]
    fn profiles_name_modes_in_the_order_kanshi_reads_them() {
        let dir = scratch("profiles");
        fs::write(
            dir.join("b"),
            "profile {\n\
             \toutput \"Unknown Unknown Unknown\" mode 1600x1000 position 0,0 enable\n\
             \toutput HEADLESS-1 mode --custom 1280x720@60Hz scale 1 enable\n\
             \toutput HEADLESS-1 mode 1024x768 mode 800x600@59.95Hz\n\
             \toutput HEADLESS-1 enable\n\
             \toutput HEADLESS-1 mode nonsense\n\
             \toutput HEADLESS-10 mode 640x480\n\
             \toutput \"Acme View 24 0\" mode 640x480\n\
             \t# output HEADLESS-1 mode 640x480\n\
             }\n",
        )
        .unwrap();
        fs::write(
            dir.join("a"),
            "profile {\n\toutput 'Unknown Unknown Unknown' mode 1920x1080@60Hz enable\n}\n",
        )
        .unwrap();
        // Repeats add nothing.
        fs::write(
            dir.join("c"),
            "profile {\n\toutput HEADLESS-1 mode 1600x1000 enable\n}\n",
        )
        .unwrap();
        fs::create_dir(dir.join("d")).unwrap();
        // Hidden, so kanshi's include leaves it out.
        fs::write(
            dir.join(".hidden"),
            "profile {\n\toutput HEADLESS-1 mode 640x480 enable\n}\n",
        )
        .unwrap();
        assert_eq!(
            profile_modes(&dir, "Unknown Unknown Unknown", "HEADLESS-1"),
            [
                (1920, 1080, 60000),
                (1600, 1000, 0),
                (1280, 720, 60000),
                (800, 600, 59950)
            ]
        );
        assert!(profile_modes(
            &dir.join("missing"),
            "Unknown Unknown Unknown",
            "HEADLESS-1"
        )
        .is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_connector_saved_for_another_monitor_is_not_this_one() {
        let dir = scratch("pinned");
        fs::write(
            dir.join("zz-other"),
            "# regolith-displayd: output \"HEADLESS-1\" is \"Acme View 24 0\"\n\
             profile {\n\
             \toutput HEADLESS-1 mode 1280x720 enable\n\
             \toutput \"Unknown Unknown Unknown\" mode 1600x1000 enable\n\
             }\n",
        )
        .unwrap();
        fs::write(
            dir.join("zz-this"),
            "# regolith-displayd: output \"HEADLESS-1\" is \"Unknown Unknown Unknown\"\n\
             profile {\n\toutput HEADLESS-1 mode 1024x768 enable\n}\n",
        )
        .unwrap();
        assert_eq!(
            profile_modes(&dir, "Unknown Unknown Unknown", "HEADLESS-1"),
            [(1600, 1000, 0), (1024, 768, 0)]
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
