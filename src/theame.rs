//! Reading `.theame` files. A `.theame` is a ZIP archive with a manifest that
//! declares any of three component kinds — mods (JS), backgrounds (images), and
//! an icon (for the Theames launcher app). A theme may provide any subset; the
//! user mixes and matches components across themes.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use zip::ZipArchive;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub mods: Vec<ModDecl>,
    #[serde(default)]
    pub backgrounds: Vec<BackgroundDecl>,
    /// CSS that themes the chat / message area, controlled separately from the
    /// background.
    #[serde(default)]
    pub chats: Vec<ChatDecl>,
    /// Path inside the archive to an icon image for the launcher app.
    #[serde(default)]
    pub icon: Option<String>,
    /// User-adjustable values (e.g. colours). Components reference them as
    /// `{{name}}` (or `{{name|rgb}}` for "r, g, b"); the user edits them in the
    /// Theames panel, so the theme itself never has to be modified.
    #[serde(default)]
    pub variables: Vec<VariableDecl>,
    /// Themes this one takes over from (their manifest names or file names). When Theames updates
    /// itself it installs this theme in their place and moves the user's choices across.
    #[serde(default)]
    pub replaces: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct VariableDecl {
    pub name: String,
    /// Label shown in the panel (defaults to the name).
    #[serde(default)]
    pub label: String,
    /// Kind of control: "color" (picker), "range" (slider, with min/max/step) or
    /// "toggle" (checkbox; the value is "1" or "0").
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,
    pub default: String,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
}

fn default_kind() -> String {
    "color".to_string()
}

#[derive(Debug, Deserialize)]
pub struct ModDecl {
    pub name: String,
    /// Path inside the archive to the mod's JS entry file.
    pub entry: String,
}

#[derive(Debug, Deserialize)]
pub struct BackgroundDecl {
    pub name: String,
    /// Path inside the archive to the image file (omit when using `color`).
    #[serde(default)]
    pub file: String,
    /// A CSS background value instead of an image, e.g. `"{{background}}"` or a
    /// gradient. Variables are substituted.
    #[serde(default)]
    pub color: Option<String>,
    /// Make Teams' opaque surfaces transparent so the background shows through
    /// the whole app. On by default — a background is invisible without it —
    /// set `"transparent": false` to keep Teams' own surfaces.
    #[serde(default = "default_true")]
    pub transparent: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct ChatDecl {
    pub name: String,
    /// Path inside the archive to the CSS file.
    pub file: String,
}

/// A loaded `.theame`: its manifest plus the archive bytes for reading entries.
pub struct Theame {
    pub file: String,
    pub manifest: Manifest,
    data: Vec<u8>,
}

impl Theame {
    pub fn open(path: &Path) -> Result<Theame> {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let data = std::fs::read(path)?;
        Theame::from_bytes(file, data)
    }

    /// A `.theame` that isn't (yet) on disk, e.g. just downloaded.
    pub fn from_bytes(file: String, data: Vec<u8>) -> Result<Theame> {
        let mut zip = ZipArchive::new(Cursor::new(&data))
            .map_err(|e| anyhow!("{file}: not a valid .theame archive ({e})"))?;
        let mut manifest_text = String::new();
        zip.by_name("manifest.json")
            .map_err(|_| anyhow!("{file}: missing manifest.json"))?
            .read_to_string(&mut manifest_text)?;
        let manifest: Manifest = serde_json::from_str(&manifest_text)
            .map_err(|e| anyhow!("{file}: invalid manifest.json ({e})"))?;

        Ok(Theame {
            file,
            manifest,
            data,
        })
    }

    pub fn read_bytes(&self, name: &str) -> Result<Vec<u8>> {
        let mut zip = ZipArchive::new(Cursor::new(&self.data))?;
        let mut entry = zip
            .by_name(name)
            .map_err(|_| anyhow!("{}: entry \"{name}\" not found", self.file))?;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        Ok(buf)
    }

    pub fn read_text(&self, name: &str) -> Result<String> {
        Ok(String::from_utf8(self.read_bytes(name)?)?)
    }

    /// Variable values for this theme: manifest defaults overridden by the
    /// user's saved choices.
    pub fn variable_values(&self, saved: Option<&HashMap<String, String>>) -> HashMap<String, String> {
        let mut vals: HashMap<String, String> = self
            .manifest
            .variables
            .iter()
            .map(|v| (v.name.clone(), v.default.clone()))
            .collect();
        if let Some(saved) = saved {
            for (k, v) in saved {
                if vals.contains_key(k) {
                    vals.insert(k.clone(), v.clone());
                }
            }
        }
        vals
    }
}

/// Is a variable's value "on"? Used by `{{#name}}` / `{{^name}}` blocks and `toggle` variables.
fn is_on(value: &str) -> bool {
    !matches!(value.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "off" | "no")
}

/// Expand conditional blocks: `{{#name}}…{{/name}}` is kept when the variable is on,
/// `{{^name}}…{{/name}}` when it is off. An unknown name counts as off. Blocks of
/// different variables may nest; a block without its closing tag is left as written.
fn apply_conditionals(text: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let sigil = after.chars().next();
        if !matches!(sigil, Some('#') | Some('^')) {
            out.push_str(&rest[..start + 2]);
            rest = after;
            continue;
        }
        let Some(name_end) = after.find("}}") else { break };
        let name = after[1..name_end].trim();
        let body_start = &after[name_end + 2..];
        let close = format!("{{{{/{name}}}}}");
        let Some(close_at) = body_start.find(&close) else {
            out.push_str(&rest[..start + 2]);
            rest = after;
            continue;
        };
        out.push_str(&rest[..start]);
        let on = vars.get(name).map(|v| is_on(v)).unwrap_or(false);
        if on == (sigil == Some('#')) {
            out.push_str(&apply_conditionals(&body_start[..close_at], vars));
        }
        rest = &body_start[close_at + close.len()..];
    }
    out.push_str(rest);
    out
}

/// Replace `{{name}}` / `{{name|rgb}}` placeholders with variable values, after
/// expanding `{{#name}}…{{/name}}` / `{{^name}}…{{/name}}` conditional blocks.
/// Unknown names are left untouched.
pub fn substitute(text: &str, vars: &HashMap<String, String>) -> String {
    if !text.contains("{{") {
        return text.to_string();
    }
    let expanded = apply_conditionals(text, vars);
    let text = expanded.as_str();
    if vars.is_empty() || !text.contains("{{") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let token = after[..end].trim();
        let (name, modifier) = match token.split_once('|') {
            Some((n, m)) => (n.trim(), m.trim()),
            None => (token, ""),
        };
        match vars.get(name) {
            Some(value) if modifier == "rgb" => out.push_str(&hex_to_rgb(value)),
            Some(value) => out.push_str(value),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// "#rrggbb" / "#rgb" -> "r, g, b". Anything else is returned unchanged.
fn hex_to_rgb(value: &str) -> String {
    let h = value.trim().trim_start_matches('#');
    let full: String = match h.len() {
        3 => h.chars().flat_map(|c| [c, c]).collect(),
        6 => h.to_string(),
        _ => return value.to_string(),
    };
    match (
        u8::from_str_radix(&full[0..2], 16),
        u8::from_str_radix(&full[2..4], 16),
        u8::from_str_radix(&full[4..6], 16),
    ) {
        (Ok(r), Ok(g), Ok(b)) => format!("{r}, {g}, {b}"),
        _ => value.to_string(),
    }
}

/// List installed `.theame` filenames in a themes dir (sorted).
pub fn list(themes_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(themes_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.to_lowercase().ends_with(".theame"))
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

pub fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn substitutes_values_and_rgb() {
        let v = vars(&[("accent", "#33ff88"), ("size", "12")]);
        assert_eq!(substitute("a{{size}}b {{accent|rgb}} {{nope}}", &v), "a12b 51, 255, 136 {{nope}}");
    }

    #[test]
    fn conditional_blocks_follow_the_toggle() {
        let on = vars(&[("t", "1")]);
        let off = vars(&[("t", "0")]);
        let css = "a{{#t}}[on]{{/t}}b{{^t}}[off]{{/t}}c";
        assert_eq!(substitute(css, &on), "a[on]bc");
        assert_eq!(substitute(css, &off), "ab[off]c");
    }

    #[test]
    fn conditionals_nest_and_use_variables_inside() {
        let v = vars(&[("a", "1"), ("b", "0"), ("x", "7")]);
        let css = "{{#a}}A{{x}}{{#b}}B{{/b}}{{^b}}!b{{/b}}{{/a}}";
        assert_eq!(substitute(css, &v), "A7!b");
    }

    #[test]
    fn unknown_toggle_is_off_and_unclosed_block_is_left_alone() {
        let v = vars(&[("x", "1")]);
        assert_eq!(substitute("p{{#nope}}q{{/nope}}r", &v), "pr");
        assert_eq!(substitute("p{{#x}}q", &v), "p{{#x}}q");
    }

    #[test]
    fn conditionals_work_with_no_variables_declared() {
        assert_eq!(substitute("a{{#t}}x{{/t}}{{^t}}y{{/t}}", &HashMap::new()), "ay");
    }
}
