//! What the server hands a browser besides the sockets: the pages, the
//! page's code, and the DE's look (its config and fonts).
//!
//! **The page.** `/` is the cce-ui app (`src/web`, built for the browser by
//! `build.rs` and embedded here). A server built with `CCE_REMOTE_SKIP_WEB=1`
//! has none, and `/` says so.
//!
//! **Caching.** The loader and the app's code are served at fixed paths
//! carrying `?v=<version>` and marked immutable, and `/` names the current
//! version, so a phone downloads the ~2 MB app once per server build, not
//! once per visit. `/` itself is `no-cache`.
//!
//! **The DE's look**, PIN-gated like `/shot` (`header_gate`): `/config` is
//! `config.kdl` (the app loads its colours and surfaces from it), `/boot`
//! names the fonts — the families the config uses, found in the DE's font
//! directory, then two system faces as fallbacks — and `/font/<n>` serves
//! them. A browser has no font directory, and the DE's own face is what makes
//! the page look like the desktop.

use std::borrow::Cow;
use std::path::PathBuf;

const INDEX: &str = include_str!("../web/index.html");
const LOADER: &str = include_str!("../web/app.js");
const GLUE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/site/cce_remote.js"));
const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/site/cce_remote_bg.wasm"));
const WASM_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/site/cce_remote_bg.wasm.gz"));

/// System faces after the DE's: a sans for the toolkit's generic family, and
/// a monospace for when the DE's own face is not found.
const FALLBACK_FONTS: [(&str, &str); 2] = [
    ("Noto Sans", "/usr/share/fonts/noto/NotoSans-Regular.ttf"),
    ("JetBrains Mono", "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf"),
];

/// Whether this build carries the cce-ui app.
pub fn built() -> bool {
    !WASM.is_empty()
}

/// Identity of everything the browser runs (FNV-1a). Sent after auth so a
/// page notices a restarted server is serving newer code and reloads, and
/// stamped on the cached assets' URLs.
pub fn version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for part in [INDEX.as_bytes(), LOADER.as_bytes(), GLUE, WASM] {
            for &b in part {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        format!("{h:016x}")
    })
}

pub struct Reply {
    pub ctype: &'static str,
    /// Further header lines, each ending in CRLF.
    pub headers: String,
    pub body: Cow<'static, [u8]>,
}

const NO_CACHE: &str = "Cache-Control: no-cache\r\n";
const IMMUTABLE: &str = "Cache-Control: public, max-age=31536000, immutable\r\n";

/// The public asset at `path` (query stripped), if there is one.
pub fn asset(path: &str, accepts_gzip: bool) -> Option<Reply> {
    let html = |body: String| Reply { ctype: "text/html; charset=utf-8", headers: NO_CACHE.into(), body: Cow::Owned(body.into_bytes()) };
    let js = |body: &'static [u8]| Reply { ctype: "text/javascript; charset=utf-8", headers: IMMUTABLE.into(), body: Cow::Borrowed(body) };
    Some(match path {
        "/" | "/index.html" if built() => html(INDEX.replace("__V__", version())),
        "/" | "/index.html" => Reply {
            ctype: "text/plain; charset=utf-8",
            headers: NO_CACHE.into(),
            body: Cow::Borrowed(b"cce-remote was built without its page (CCE_REMOTE_SKIP_WEB=1).\n"),
        },
        "/app.js" if built() => js(LOADER.as_bytes()),
        "/cce_remote.js" if built() => js(GLUE),
        "/cce_remote_bg.wasm" if built() => {
            // A wasm module must arrive as application/wasm to be compiled
            // while it streams in.
            let (headers, body) = if accepts_gzip { (format!("{IMMUTABLE}Content-Encoding: gzip\r\n"), WASM_GZ) } else { (IMMUTABLE.into(), WASM) };
            Reply { ctype: "application/wasm", headers, body: Cow::Borrowed(body) }
        }
        _ => return None,
    })
}

fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
    });
    base.join("cce").join("config.kdl")
}

/// `config.kdl`, or nothing (the app then draws in the toolkit's defaults).
pub fn config() -> String {
    std::fs::read_to_string(config_path()).unwrap_or_default()
}

/// `/boot`: `families <serif>,<sans>,<mono>` and one `font /font/<n>` line
/// per face, in the order the app should load them.
pub fn boot() -> String {
    let (families, files) = fonts();
    let mut out = format!("families {families}\n");
    for (n, _) in files.iter().enumerate() {
        out.push_str(&format!("font /font/{n}?v={}\n", version()));
    }
    out
}

/// `/font/<n>`: the face's bytes and content type.
pub fn font(n: usize) -> Option<(Vec<u8>, &'static str)> {
    let (_, files) = fonts();
    let path = files.get(n)?;
    let ctype = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("otf")) { "font/otf" } else { "font/ttf" };
    Some((std::fs::read(path).ok()?, ctype))
}

/// The faces to send and the generic families they make: the DE's families
/// (the config's `font=` values) found in its font directory, then
/// `FALLBACK_FONTS`. The monospace generic is the DE's first face, as the DE
/// sets everything in one.
fn fonts() -> (String, Vec<PathBuf>) {
    let dir = std::env::var("CCE_FONTS_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Dropbox/Fonts")
    });
    let candidates = font_files(&dir);
    let mut files = Vec::new();
    let mut mono = None;
    for family in config_families(&config()) {
        if let Some(path) = candidates.iter().find(|p| p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| font_matches(&family, s))) {
            mono.get_or_insert(family);
            files.push(path.clone());
        }
    }
    let mut sans = None;
    for (family, path) in FALLBACK_FONTS {
        let path = PathBuf::from(path);
        if path.is_file() {
            if family == "Noto Sans" {
                sans = Some(family.to_string());
            } else {
                mono.get_or_insert(family.to_string());
            }
            files.push(path);
        }
    }
    (format!(",{},{}", sans.unwrap_or_default(), mono.unwrap_or_default()), files)
}

/// Font files in `dir` and its subdirectories (one level), sorted.
fn font_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let is_font = |p: &PathBuf| p.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "ttf" | "otf"));
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(std::fs::read_dir(&p).into_iter().flatten().flatten().map(|e| e.path()).filter(is_font));
        } else if is_font(&p) {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// The font families a config names, in first-use order: each `font="<family>
/// <size>"` (or `font "<family> <size>"`) with the size dropped.
fn config_families(config: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, _) in config.match_indices("font") {
        // `font` as a whole word, then `=` or a space, then a quoted string.
        let before = config[..i].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let rest = config[i + 4..].trim_start_matches([' ', '=']);
        let Some(rest) = rest.strip_prefix('"') else { continue };
        let Some(value) = rest.split('"').next() else { continue };
        let family = match value.rsplit_once(' ') {
            Some((name, size)) if size.parse::<f32>().is_ok() => name,
            _ => value,
        }
        .trim();
        if !family.is_empty() && !out.iter().any(|f| f == family) {
            out.push(family.to_string());
        }
    }
    out
}

/// Whether a font file's stem is `family`'s regular face: `Berkeley Mono`
/// matches `BerkeleyMono-Regular` and `BerkeleyMono`, not `BerkeleyMono-Bold`.
fn font_matches(family: &str, stem: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let (family, stem) = (norm(family), norm(stem));
    stem == family || stem == format!("{family}regular")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_come_from_font_values_without_their_sizes() {
        let config = r#"
            button font="Berkeley Mono 14" padding=(i64)9
            status font="Chivo Mono 14"
            menubar font="Berkeley Mono 12"
            heading { font "Inter Display 18"; }
            fonts { }
            font_selector font="Berkeley Mono 14"
            label font_detached="Ignored 14"
        "#;
        assert_eq!(config_families(config), ["Berkeley Mono", "Chivo Mono", "Inter Display"]);
    }

    #[test]
    fn a_family_matches_its_regular_face_only() {
        assert!(font_matches("Berkeley Mono", "BerkeleyMono-Regular"));
        assert!(font_matches("Berkeley Mono", "BerkeleyMono"));
        assert!(font_matches("Chivo Mono", "chivo_mono_regular"));
        assert!(!font_matches("Berkeley Mono", "BerkeleyMono-Bold"));
        assert!(!font_matches("Berkeley Mono", "BerkeleyMono-Oblique"));
        assert!(!font_matches("Mono", "BerkeleyMono-Regular"));
    }

    #[test]
    fn the_app_is_cached_by_version_and_the_page_is_not() {
        if !built() {
            return;
        }
        let index = asset("/", true).unwrap();
        assert!(index.headers.contains("no-cache"));
        let body = String::from_utf8(index.body.into_owned()).unwrap();
        assert!(body.contains(version()) && !body.contains("__V__"), "/ names the current version");
        let wasm = asset("/cce_remote_bg.wasm", true).unwrap();
        assert_eq!(wasm.ctype, "application/wasm");
        assert!(wasm.headers.contains("immutable") && wasm.headers.contains("gzip"));
        assert!(!asset("/cce_remote_bg.wasm", false).unwrap().headers.contains("gzip"));
        assert!(asset("/nope", true).is_none());
    }
}
