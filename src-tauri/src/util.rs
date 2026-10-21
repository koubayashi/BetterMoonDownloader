//! Small helpers shared by the extractors and the download engine.

use percent_encoding::percent_decode_str;
use url::Url;

pub const FUCKINGFAST_HOST: &str = "fuckingfast.co";
pub const DATANODES_HOST: &str = "datanodes.to";

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Host {
    Fuckingfast,
    Datanodes,
    Unsupported,
}

impl Host {
    pub fn of(url: &str) -> Host {
        let host = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_default();
        let base = host.strip_prefix("www.").unwrap_or(&host);
        if base == FUCKINGFAST_HOST || base.ends_with(".fuckingfast.co") {
            Host::Fuckingfast
        } else if base == DATANODES_HOST || base.ends_with(".datanodes.to") {
            Host::Datanodes
        } else {
            Host::Unsupported
        }
    }
}

/// Pull every http(s) URL out of free text: one per line, but tolerant of
/// pasted HTML, bullets, quotes and several links on one line.
pub fn parse_links(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"https?://[^\s"'<>\]\)]+"#).unwrap();
    let mut out: Vec<String> = Vec::new();
    for m in re.find_iter(text) {
        let u = m.as_str().trim_end_matches([',', ';', '.']).to_string();
        if !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

/// Filename for a share link. FitGirl-style links carry it as a URL fragment
/// (`https://fuckingfast.co/abc123#Game.part01.rar`); datanodes as the last path
/// segment (`/code/Game.part01.rar`).
pub fn filename_for(url: &str) -> String {
    let raw = match Url::parse(url) {
        Ok(u) => {
            if let Some(f) = u.fragment().filter(|f| !f.is_empty()) {
                f.to_string()
            } else {
                u.path_segments()
                    .and_then(|s| s.filter(|p| !p.is_empty()).last().map(str::to_string))
                    .unwrap_or_default()
            }
        }
        Err(_) => String::new(),
    };
    let decoded = percent_decode_str(&raw).decode_utf8_lossy().to_string();
    sanitize_filename(if decoded.is_empty() { "download" } else { &decoded })
}

/// Longest name we produce, in UTF-16 units. NTFS allows 255; this leaves room
/// for `.bmdpart.json` and an `.old2` tag.
const MAX_NAME: usize = 200;

/// Text-direction controls (e.g. U+202E) let `Game<U+202E>rar.exe` display as `Gameexe.rar`.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// A name Windows (and Explorer) will treat as an ordinary file: no reserved
/// characters, no direction tricks, no device names, no over-long names.
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !is_bidi_control(*c))
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let mut s = cleaned.trim_matches(|c| c == '.' || c == ' ').to_string();
    if s.is_empty() {
        s = "download".into();
    }

    // CON, NUL.rar, COM1.part1.rar, …: created through \\?\ paths but
    // unopenable and undeletable in Explorer and most tools.
    let stem = s.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.chars().count() == 4
            && stem.chars().nth(3).is_some_and(|d| d.is_ascii_digit() || "¹²³".contains(d)));
    if reserved {
        s.insert(0, '_');
    }

    // Shorten the stem, never the extension (`.part01.rar` counts as one).
    if s.encode_utf16().count() > MAX_NAME {
        let ext_re = regex::Regex::new(r"(?i)(\.part\d+)?\.[a-z0-9]{1,8}$").unwrap();
        let ext = ext_re.find(&s).map(|m| m.as_str().to_string()).unwrap_or_default();
        let mut stem: String = s[..s.len() - ext.len()].to_string();
        while stem.encode_utf16().count() + ext.encode_utf16().count() > MAX_NAME {
            stem.pop();
        }
        s = format!("{}{ext}", stem.trim_end_matches(['.', ' ']));
    }
    s
}

/// The release a part belongs to, used as the group and its subfolder name.
/// `ExampleGame_--_fitgirl-repacks.site_--_.part1.rar` → `ExampleGame`,
/// `Some.Game.v1.2.part03.rar` → `Some Game v1.2`, `Movie.2024.mkv` → `Movie 2024`.
pub fn release_name(filename: &str) -> String {
    use regex::Regex;
    let mut s = filename.to_string();
    // Peel archive/part suffixes: .part01.rar, .r00, .7z.001, .zip, .z01, ...
    let suffix = Regex::new(
        r"(?i)(\.part\d+)?\.(rar|zip|7z|r\d{2,3}|z\d{2}|\d{3}|bin|iso|tar|gz|xz|exe|mkv|mp4|avi)$",
    )
    .unwrap();
    for _ in 0..3 {
        let next = suffix.replace(&s, "").to_string();
        if next == s {
            break;
        }
        s = next;
    }
    // Uploader tags like `_--_fitgirl-repacks.site_--_` carry no meaning.
    let tag = Regex::new(r"(?i)[\s._-]*--[\s._-]*[a-z0-9-]+\.(site|com|net|org|to|cc)[\s._-]*--[\s._-]*").unwrap();
    s = tag.replace_all(&s, " ").to_string();
    // Dots and underscores are word separators, except a dot between two
    // digits (version numbers like 1.2).
    let chars: Vec<char> = s.chars().collect();
    let out: String = chars
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let between_digits = i > 0
                && chars[i - 1].is_ascii_digit()
                && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit());
            match c {
                '_' => ' ',
                '.' if !between_digits => ' ',
                c => c,
            }
        })
        .collect();
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let out = out.trim_matches(|c: char| c == '-' || c == ' ').to_string();
    sanitize_filename(if out.is_empty() { filename } else { &out })
}

/// The Referer each CDN expects for an extracted URL.
pub fn referer_for(direct: &str) -> &'static str {
    let host = Url::parse(direct)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    if host.contains("fuckingfast") {
        "https://fuckingfast.co/"
    } else {
        "https://datanodes.to/"
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_fragment_and_path() {
        assert_eq!(
            filename_for("https://fuckingfast.co/smeekt12mped#Game.part01.rar"),
            "Game.part01.rar"
        );
        assert_eq!(
            filename_for("https://datanodes.to/abcd1234efgh/My%20Game.part02.rar"),
            "My Game.part02.rar"
        );
        assert_eq!(sanitize_filename("a:b?.rar. "), "a_b_.rar");
    }

    #[test]
    fn hostile_filenames_are_neutralised() {
        // L2: direction override would show "rar.exe" as "exe.rar".
        assert_eq!(sanitize_filename("Game\u{202E}rar.exe"), "Gamerar.exe");
        // L3: device names.
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("nul.rar"), "_nul.rar");
        assert_eq!(sanitize_filename("COM1.part1.rar"), "_COM1.part1.rar");
        assert_eq!(sanitize_filename("Console.rar"), "Console.rar");
        // L4: long names keep their extension and fit NTFS.
        let long = sanitize_filename(&format!("{}.part07.rar", "A".repeat(400)));
        assert!(long.encode_utf16().count() <= 200, "{}", long.len());
        assert!(long.ends_with(".part07.rar"));
        // Traversal and streams stay neutralised.
        assert_eq!(sanitize_filename("../../Windows/evil.dll"), "_.._Windows_evil.dll");
        assert_eq!(sanitize_filename("file.rar:hidden"), "file.rar_hidden");
    }

    #[test]
    fn release_names() {
        assert_eq!(release_name("ExampleGame_--_fitgirl-repacks.site_--_.part1.rar"), "ExampleGame");
        assert_eq!(release_name("Some.Game.v1.2.part03.rar"), "Some Game v1.2");
        assert_eq!(release_name("Elden_Ring_Deluxe.7z.001"), "Elden Ring Deluxe");
        assert_eq!(release_name("Movie.2024.mkv"), "Movie 2024");
        assert_eq!(release_name("setup-fitgirl-01.bin"), "setup-fitgirl-01");
    }

    #[test]
    fn links_parse_and_dedupe() {
        let t = "1. https://fuckingfast.co/abc#x.rar\nhttps://fuckingfast.co/abc#x.rar, <a href=\"https://datanodes.to/q/y.rar\">";
        assert_eq!(parse_links(t).len(), 2);
        assert_eq!(Host::of("https://www.datanodes.to/x"), Host::Datanodes);
        assert_eq!(Host::of("https://example.com/x"), Host::Unsupported);
    }
}
