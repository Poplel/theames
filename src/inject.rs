//! Resolving the current selection into structured theme data: the chosen
//! background, the chosen chat CSS, and each selected mod's source kept
//! separate (so the page can enable/disable mods individually and reversibly).
//! Theme variables (`{{name}}`) are substituted here, per theme, so the same
//! variable name in two themes never collides.

use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use crate::config::Config;
use crate::theame::{mime_for, substitute, Theame};

#[derive(Serialize)]
pub struct Theme {
    /// Background image as a data URI, or "" for none / a colour background.
    pub background: String,
    /// CSS `background` value for a colour/gradient background, or "".
    pub background_color: String,
    /// Whether the selected background wants Teams' surfaces made transparent.
    pub transparent: bool,
    /// Chat-area CSS, or "" for none.
    pub chats: String,
    /// Selected mods, each with its own source.
    pub mods: Vec<ModScript>,
}

#[derive(Serialize)]
pub struct ModScript {
    /// The selection reference, e.g. "file.theame::ModName".
    pub name: String,
    pub source: String,
}

/// Split "file.theame::Component" into (file, component). With no "::", the
/// whole string is the file and the component name is empty.
fn split_ref(reference: &str) -> (&str, &str) {
    match reference.split_once("::") {
        Some((file, name)) => (file, name),
        None => (reference, ""),
    }
}

/// Variable values for one theme file (defaults + the user's saved choices).
fn vars_for(cfg: &Config, theame: &Theame, file: &str) -> HashMap<String, String> {
    theame.variable_values(cfg.variables.get(file))
}

struct Bg {
    data_uri: String,
    color: String,
    transparent: bool,
}

fn resolve_background(themes_dir: &Path, cfg: &Config) -> Result<Bg> {
    let none = Bg { data_uri: String::new(), color: String::new(), transparent: false };
    let Some(reference) = &cfg.selection.background else {
        return Ok(none);
    };
    let (file, name) = split_ref(reference);
    let theame = Theame::open(&themes_dir.join(file))?;
    let Some(bg) = theame.manifest.backgrounds.iter().find(|b| b.name == name) else {
        eprintln!("[theames] background \"{name}\" not found in {file}");
        return Ok(none);
    };
    if let Some(color) = &bg.color {
        let vars = vars_for(cfg, &theame, file);
        return Ok(Bg { data_uri: String::new(), color: substitute(color, &vars), transparent: bg.transparent });
    }
    let bytes = theame.read_bytes(&bg.file)?;
    Ok(Bg {
        data_uri: format!("data:{};base64,{}", mime_for(&bg.file), STANDARD.encode(bytes)),
        color: String::new(),
        transparent: bg.transparent,
    })
}

/// Resolve the selected chat CSS theme, or empty string if none.
fn chats_css(themes_dir: &Path, cfg: &Config) -> Result<String> {
    let Some(reference) = &cfg.selection.chats else {
        return Ok(String::new());
    };
    let (file, name) = split_ref(reference);
    let theame = Theame::open(&themes_dir.join(file))?;
    let Some(chat) = theame.manifest.chats.iter().find(|c| c.name == name) else {
        eprintln!("[theames] chat theme \"{name}\" not found in {file}");
        return Ok(String::new());
    };
    let vars = vars_for(cfg, &theame, file);
    Ok(substitute(&theame.read_text(&chat.file)?, &vars))
}

/// Read the source of every selected mod, keeping each one separate.
fn mod_scripts(themes_dir: &Path, cfg: &Config) -> Result<Vec<ModScript>> {
    let mut out = Vec::new();
    for reference in &cfg.selection.mods {
        let (file, name) = split_ref(reference);
        let theame = match Theame::open(&themes_dir.join(file)) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[theames] skipping mod {reference}: {e}");
                continue;
            }
        };
        let Some(decl) = theame.manifest.mods.iter().find(|m| m.name == name) else {
            eprintln!("[theames] mod \"{name}\" not found in {file}");
            continue;
        };
        let vars = vars_for(cfg, &theame, file);
        out.push(ModScript {
            name: reference.clone(),
            source: substitute(&theame.read_text(&decl.entry)?, &vars),
        });
    }
    Ok(out)
}

/// Resolve the current selection into theme data for the page to apply.
pub fn build_theme(themes_dir: &Path, cfg: &Config) -> Result<Theme> {
    let bg = resolve_background(themes_dir, cfg)?;
    Ok(Theme {
        background: bg.data_uri,
        background_color: bg.color,
        transparent: bg.transparent,
        chats: chats_css(themes_dir, cfg)?,
        mods: mod_scripts(themes_dir, cfg)?,
    })
}
