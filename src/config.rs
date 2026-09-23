//! Remembers what was used the last time, so the startup window can offer it again: the
//! ColourSpace address and the HDR choices (checkbox, signal, primaries and the four numbers).
//!
//! Everything is kept in one plain-text file, `calibrationclient.settings`, in the folder the
//! program is started from (the current directory) and nowhere else. Start the program from
//! another folder and that folder has its own file (or none, and then its own defaults). Every
//! failure is non-fatal: without a usable file the program simply starts with its built-in
//! defaults.

use std::fs;
use std::path::{Path, PathBuf};

/// Name of the file, in the current directory.
pub const FILE_NAME: &str = "calibrationclient.settings";

/// Longest address accepted (the startup window's limit).
pub const MAX_ADDRESS_LEN: usize = 64;

/// Characters an address may contain: host names, IPv4, IPv6 (with brackets) and ports.
pub fn is_address_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | ':' | '-' | '_' | '[' | ']')
}

/// A usable saved address: not empty, not too long, only address characters.
pub fn is_valid_address(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ADDRESS_LEN && s.chars().all(is_address_char)
}

/// Everything that can be remembered. A field is `None` when nothing (valid) was saved for it.
/// Text values are the same words the command line uses; the caller decides what they mean.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Saved {
    pub ip: Option<String>,
    /// The HDR checkbox.
    pub hdr_enabled: Option<bool>,
    /// "hdr10" or "hlg": the Signal menu.
    pub signal: Option<String>,
    /// "bt2020" or "p3d65".
    pub primaries: Option<String>,
    pub max_luminance: Option<f32>,
    pub min_luminance: Option<f32>,
    pub max_cll: Option<f32>,
    pub max_fall: Option<f32>,
}

/// A saved number must be a plain finite value in a sane range.
fn parse_number(v: &str) -> Option<f32> {
    v.parse::<f32>().ok().filter(|n| n.is_finite() && (0.0..=1_000_000.0).contains(n))
}

/// A saved word: short, letters and digits only.
fn parse_word(v: &str) -> Option<String> {
    let ok = !v.is_empty() && v.len() <= 16 && v.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then(|| v.to_ascii_lowercase())
}

impl Saved {
    /// Read `key=value` lines. Unknown keys, bad values, blank lines and `#` comments are
    /// skipped, so a damaged or hand-edited file can never do harm.
    fn parse(text: &str) -> Saved {
        let mut saved = Saved::default();
        for line in text.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            match key.as_str() {
                "ip" => saved.ip = is_valid_address(value).then(|| value.to_string()),
                "hdr_enabled" => {
                    saved.hdr_enabled = match value.to_ascii_lowercase().as_str() {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    }
                }
                "signal" => saved.signal = parse_word(value),
                "primaries" => saved.primaries = parse_word(value),
                "max_luminance" => saved.max_luminance = parse_number(value),
                "min_luminance" => saved.min_luminance = parse_number(value),
                "max_cll" => saved.max_cll = parse_number(value),
                "max_fall" => saved.max_fall = parse_number(value),
                _ => {}
            }
        }
        saved
    }

    fn render(&self) -> String {
        let mut out = String::from("# calibrationclient: remembered from the last connection (edited by the program)\n");
        let mut put = |key: &str, value: Option<String>| {
            if let Some(v) = value {
                out.push_str(&format!("{key}={v}\n"));
            }
        };
        put("ip", self.ip.clone());
        put("hdr_enabled", self.hdr_enabled.map(|b| b.to_string()));
        put("signal", self.signal.clone());
        put("primaries", self.primaries.clone());
        put("max_luminance", self.max_luminance.map(|n| n.to_string()));
        put("min_luminance", self.min_luminance.map(|n| n.to_string()));
        put("max_cll", self.max_cll.map(|n| n.to_string()));
        put("max_fall", self.max_fall.map(|n| n.to_string()));
        out
    }
}

/// The settings file in the current directory.
fn local_path() -> Option<PathBuf> {
    std::env::current_dir().ok().map(|dir| dir.join(FILE_NAME))
}

/// Missing or unreadable files give all `None`s.
fn read_saved(path: &Path) -> Saved {
    fs::read_to_string(path).map(|text| Saved::parse(&text)).unwrap_or_default()
}

fn write_saved(path: &Path, saved: &Saved) -> std::io::Result<()> {
    // Write next to it and rename, so a crash can never leave a half-written file behind.
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, saved.render())?;
    fs::rename(&tmp, path)
}

/// What was remembered in the current folder (all `None`s if there is nothing usable).
pub fn load() -> Saved {
    local_path().map(|path| read_saved(&path)).unwrap_or_default()
}

/// Remember `saved` in the current folder. Failures are only reported, never fatal.
pub fn save(saved: &Saved) {
    let Some(path) = local_path() else { return };
    if let Err(e) = write_saved(&path, saved) {
        eprintln!("Could not save the settings to {}: {}", path.display(), e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh scratch directory, removed at the end of the test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("calibrationclient-test-{}-{}", std::process::id(), name));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn everything() -> Saved {
        Saved {
            ip: Some("192.168.168.207".into()),
            hdr_enabled: Some(true),
            signal: Some("hlg".into()),
            primaries: Some("p3d65".into()),
            max_luminance: Some(1200.0),
            min_luminance: Some(0.0005),
            max_cll: Some(0.0),
            max_fall: Some(400.5),
        }
    }

    #[test]
    fn everything_round_trips_through_the_file() {
        let dir = Scratch::new("roundtrip");
        let path = dir.0.join(FILE_NAME);
        assert_eq!(read_saved(&path), Saved::default(), "nothing saved yet");

        write_saved(&path, &everything()).unwrap();
        assert_eq!(read_saved(&path), everything());
        assert!(!path.with_extension("tmp").exists(), "no temp file left behind");

        let only_ip = Saved { ip: Some("[::1]:20002".into()), ..Default::default() };
        write_saved(&path, &only_ip).unwrap(); // overwritten, and missing fields stay missing
        assert_eq!(read_saved(&path), only_ip);
    }

    #[test]
    fn the_file_is_only_ever_the_one_in_the_current_folder() {
        let here = std::env::current_dir().unwrap();
        assert_eq!(local_path(), Some(here.join(FILE_NAME)));
        assert_eq!(FILE_NAME, "calibrationclient.settings");
    }

    #[test]
    fn saving_fails_quietly_when_the_folder_cannot_be_written() {
        let dir = Scratch::new("unwritable");
        let blocker = dir.0.join("blocker");
        fs::write(&blocker, "x").unwrap(); // a file, so nothing can be created "inside" it
        assert!(write_saved(&blocker.join(FILE_NAME), &everything()).is_err());
        assert_eq!(read_saved(&blocker.join(FILE_NAME)), Saved::default());
    }

    #[test]
    fn bad_lines_and_values_are_ignored_one_by_one() {
        let text = "\
            # a comment\n\
            ip = 10.0.0.2 \n\
            hdr_enabled=maybe\n\
            signal=HLG\n\
            primaries=../../etc\n\
            max_luminance=abc\n\
            min_luminance=-1\n\
            max_cll=nan\n\
            max_fall=99999999\n\
            unknown_key=1\n\
            no equals sign here\n";
        assert_eq!(
            Saved::parse(text),
            Saved { ip: Some("10.0.0.2".into()), signal: Some("hlg".into()), ..Default::default() }
        );
        assert_eq!(Saved::parse("ip=evil;rm -rf").ip, None);
        assert_eq!(Saved::parse(&format!("ip={}", "9".repeat(100))).ip, None);
        assert_eq!(Saved::parse("\u{0}\u{1}garbage"), Saved::default());
        assert_eq!(Saved::parse("hdr_enabled=TRUE").hdr_enabled, Some(true));
    }

    #[test]
    fn only_sane_addresses_are_accepted() {
        for ok in ["192.168.1.5", "192.168.1.5:20002", "colourspace-pc", "my_pc.local", "::1", "[fe80::1]:5000"] {
            assert!(is_valid_address(ok), "{ok}");
        }
        for bad in ["", " ", "a b", "a\nb", "a/b", "a;b", "x'y", &"a".repeat(65)] {
            assert!(!is_valid_address(bad), "{bad:?}");
        }
    }
}
