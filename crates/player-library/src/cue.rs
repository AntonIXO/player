//! Minimal CUE-sheet parser — enough to split one (or more) audio files into
//! per-track entries with start offsets. Hand-rolled (no extra dependency); it
//! reads the commands we care about (`TITLE`, `PERFORMER`, `FILE`, `TRACK`,
//! `INDEX 01`, plus `REM DATE`/`REM GENRE`) and ignores the rest.

use std::path::Path;

/// One track entry inside a CUE sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueTrack {
    pub number: u32,
    pub title: Option<String>,
    pub performer: Option<String>,
    /// Start offset (from `INDEX 01`) into the owning file, in milliseconds.
    pub start_ms: u64,
}

/// An audio `FILE` and the run of tracks that index into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueFile {
    /// File name exactly as written in the sheet (resolved against the `.cue`'s
    /// directory by the caller).
    pub name: String,
    pub tracks: Vec<CueTrack>,
}

/// A parsed CUE sheet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CueSheet {
    pub title: Option<String>,
    pub performer: Option<String>,
    pub genre: Option<String>,
    pub date: Option<i32>,
    pub files: Vec<CueFile>,
}

/// Read and parse a `.cue` file. Returns `None` if it can't be read or yields no
/// tracks.
pub fn parse_file(path: &Path) -> Option<CueSheet> {
    let bytes = std::fs::read(path).ok()?;
    let sheet = CueSheet::parse(&decode_text(&bytes));
    if sheet.files.iter().all(|f| f.tracks.is_empty()) {
        return None;
    }
    Some(sheet)
}

impl CueSheet {
    pub fn parse(text: &str) -> Self {
        let mut sheet = CueSheet::default();
        let mut cur_file: Option<CueFile> = None;
        let mut cur_track: Option<CueTrack> = None;

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let (cmd, rest) = split_first(line);
            match cmd.to_ascii_uppercase().as_str() {
                "REM" => {
                    let (k, v) = split_first(rest);
                    match k.to_ascii_uppercase().as_str() {
                        "DATE" => sheet.date = parse_year(&unquote(v)),
                        "GENRE" => sheet.genre = non_empty(unquote(v)),
                        _ => {}
                    }
                }
                // TITLE/PERFORMER attach to the current track once one is open,
                // else to the album (they appear before the first TRACK).
                "TITLE" => {
                    let t = non_empty(unquote(rest));
                    match cur_track.as_mut() {
                        Some(tr) => tr.title = t,
                        None => sheet.title = t,
                    }
                }
                "PERFORMER" => {
                    let p = non_empty(unquote(rest));
                    match cur_track.as_mut() {
                        Some(tr) => tr.performer = p,
                        None => sheet.performer = p,
                    }
                }
                "FILE" => {
                    flush_track(&mut cur_file, &mut cur_track);
                    if let Some(f) = cur_file.take() {
                        sheet.files.push(f);
                    }
                    cur_file = Some(CueFile {
                        name: file_name(rest),
                        tracks: Vec::new(),
                    });
                }
                "TRACK" => {
                    flush_track(&mut cur_file, &mut cur_track);
                    let (num, _ty) = split_first(rest);
                    cur_track = Some(CueTrack {
                        number: num.trim().parse().unwrap_or(0),
                        title: None,
                        performer: None,
                        start_ms: 0,
                    });
                }
                "INDEX" => {
                    // INDEX 01 is the track's audible start; INDEX 00 (pregap) is
                    // ignored.
                    let (idx, time) = split_first(rest);
                    if idx.trim() == "01" {
                        if let Some(tr) = cur_track.as_mut() {
                            tr.start_ms = parse_index_ms(time.trim());
                        }
                    }
                }
                _ => {}
            }
        }
        flush_track(&mut cur_file, &mut cur_track);
        if let Some(f) = cur_file.take() {
            sheet.files.push(f);
        }
        sheet
    }
}

fn flush_track(file: &mut Option<CueFile>, track: &mut Option<CueTrack>) {
    if let (Some(f), Some(t)) = (file.as_mut(), track.take()) {
        f.tracks.push(t);
    }
}

/// Split off the first whitespace-delimited token; returns `(token, remainder)`
/// with the remainder trimmed.
fn split_first(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

/// Strip a single pair of surrounding double quotes (and surrounding space).
fn unquote(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

fn non_empty(s: String) -> Option<String> {
    (!s.trim().is_empty()).then(|| s.trim().to_string())
}

/// Extract the file name from a `FILE "name" TYPE` line (quoted name preferred;
/// otherwise drop a trailing format token like `WAVE`).
fn file_name(rest: &str) -> String {
    let r = rest.trim();
    if let Some(after) = r.strip_prefix('"') {
        if let Some(end) = after.find('"') {
            return after[..end].to_string();
        }
    }
    const TYPES: &[&str] = &["WAVE", "MP3", "AIFF", "FLAC", "BINARY", "MOTOROLA"];
    if let Some((name, last)) = r.rsplit_once(char::is_whitespace) {
        if TYPES.contains(&last.to_ascii_uppercase().as_str()) {
            return name.trim().to_string();
        }
    }
    r.to_string()
}

/// `mm:ss:ff` (ff = CD frames, 75 per second) → milliseconds.
fn parse_index_ms(t: &str) -> u64 {
    let mut it = t.split(':');
    let mm: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let ss: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let ff: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    mm * 60_000 + ss * 1_000 + ff * 1_000 / 75
}

/// First four-digit run as a year.
fn parse_year(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(3))
        .find(|&i| b[i..i + 4].iter().all(u8::is_ascii_digit))
        .and_then(|i| s[i..i + 4].parse().ok())
}

/// Decode CUE bytes to text: honour a UTF-8 BOM, otherwise lossy UTF-8 (CUE
/// sheets are usually ASCII/UTF-8; exotic encodings degrade gracefully).
fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_to_ms() {
        assert_eq!(parse_index_ms("00:00:00"), 0);
        assert_eq!(parse_index_ms("09:22:00"), 562_000);
        // 37 frames = 37/75 s = 493.33 ms → 493 (integer).
        assert_eq!(parse_index_ms("01:30:37"), 90_000 + 493);
    }

    #[test]
    fn parses_single_file_sheet() {
        let text = r#"REM GENRE Jazz
REM DATE 1959
PERFORMER "Miles Davis"
TITLE "Kind of Blue"
FILE "Kind of Blue.flac" WAVE
  TRACK 01 AUDIO
    TITLE "So What"
    PERFORMER "Miles Davis"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Freddie Freeloader"
    INDEX 01 09:22:00
"#;
        let s = CueSheet::parse(text);
        assert_eq!(s.title.as_deref(), Some("Kind of Blue"));
        assert_eq!(s.performer.as_deref(), Some("Miles Davis"));
        assert_eq!(s.date, Some(1959));
        assert_eq!(s.genre.as_deref(), Some("Jazz"));
        assert_eq!(s.files.len(), 1);
        let f = &s.files[0];
        assert_eq!(f.name, "Kind of Blue.flac");
        assert_eq!(f.tracks.len(), 2);
        assert_eq!(f.tracks[0].title.as_deref(), Some("So What"));
        assert_eq!(f.tracks[0].start_ms, 0);
        assert_eq!(f.tracks[1].title.as_deref(), Some("Freddie Freeloader"));
        assert_eq!(f.tracks[1].start_ms, 562_000);
    }
}
