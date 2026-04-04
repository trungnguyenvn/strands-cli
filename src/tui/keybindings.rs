//! Configurable keybinding system — mirrors Claude Code's keybinding architecture.
//!
//! Loads user overrides from `~/.strands/keybindings.json`.
//! Falls back to built-in defaults.

#![allow(dead_code)]

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyModifiers};

/// A key chord: modifiers + key code.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub modifiers: KeyModifiers,
    pub code: KeyCode,
}

impl KeyChord {
    pub const fn new(modifiers: KeyModifiers, code: KeyCode) -> Self {
        Self { modifiers, code }
    }

    pub fn matches(&self, modifiers: KeyModifiers, code: KeyCode) -> bool {
        self.modifiers == modifiers && self.code == code
    }
}

/// Named actions that can be bound to key chords.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Quit,
    Cancel,
    Dismiss,
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollLineUp,
    ScrollLineDown,
    AutocompleteAccept,
    AutocompletePrevious,
    AutocompleteNext,
    Submit,
    Newline,
    HistoryPrevious,
    HistoryNext,
    ToggleVimMode,
    PermissionAllow,
    PermissionDeny,
}

/// Map of action → key chord(s). Multiple chords can trigger the same action.
pub type KeybindingMap = HashMap<Action, Vec<KeyChord>>;

/// Build the default keybinding map.
pub fn default_keybindings() -> KeybindingMap {
    let mut map = KeybindingMap::new();

    map.insert(Action::Quit, vec![
        KeyChord::new(KeyModifiers::CONTROL, KeyCode::Char('c')),
    ]);
    map.insert(Action::Cancel, vec![
        KeyChord::new(KeyModifiers::NONE, KeyCode::Esc),
    ]);
    map.insert(Action::ScrollPageUp, vec![
        KeyChord::new(KeyModifiers::NONE, KeyCode::PageUp),
    ]);
    map.insert(Action::ScrollPageDown, vec![
        KeyChord::new(KeyModifiers::NONE, KeyCode::PageDown),
    ]);
    map.insert(Action::ScrollLineUp, vec![
        KeyChord::new(KeyModifiers::SHIFT, KeyCode::Up),
    ]);
    map.insert(Action::ScrollLineDown, vec![
        KeyChord::new(KeyModifiers::SHIFT, KeyCode::Down),
    ]);
    map.insert(Action::AutocompleteAccept, vec![
        KeyChord::new(KeyModifiers::NONE, KeyCode::Tab),
    ]);
    map.insert(Action::ToggleVimMode, vec![
        KeyChord::new(KeyModifiers::CONTROL, KeyCode::Char('v')),
    ]);
    map.insert(Action::PermissionAllow, vec![
        KeyChord::new(KeyModifiers::NONE, KeyCode::Char('y')),
    ]);
    map.insert(Action::PermissionDeny, vec![
        KeyChord::new(KeyModifiers::NONE, KeyCode::Char('n')),
    ]);

    map
}

/// Resolve an action from key modifiers + code using the keybinding map.
pub fn resolve_action(map: &KeybindingMap, modifiers: KeyModifiers, code: KeyCode) -> Option<Action> {
    for (action, chords) in map {
        for chord in chords {
            if chord.matches(modifiers, code) {
                return Some(action.clone());
            }
        }
    }
    None
}

/// JSON format for user keybinding overrides.
#[derive(serde::Deserialize, Default)]
struct KeybindingFile {
    #[serde(default)]
    bindings: Vec<KeybindingEntry>,
}

#[derive(serde::Deserialize)]
struct KeybindingEntry {
    action: Action,
    key: String,
    #[serde(default)]
    modifiers: Vec<String>,
}

fn parse_key(key: &str) -> Option<KeyCode> {
    match key.to_lowercase().as_str() {
        "enter" | "return" => Some(KeyCode::Enter),
        "esc" | "escape" => Some(KeyCode::Esc),
        "tab" => Some(KeyCode::Tab),
        "backspace" => Some(KeyCode::Backspace),
        "delete" | "del" => Some(KeyCode::Delete),
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "pageup" => Some(KeyCode::PageUp),
        "pagedown" => Some(KeyCode::PageDown),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        s if s.len() == 1 => Some(KeyCode::Char(s.chars().next().unwrap())),
        _ => None,
    }
}

fn parse_modifiers(mods: &[String]) -> KeyModifiers {
    let mut result = KeyModifiers::NONE;
    for m in mods {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" => result |= KeyModifiers::CONTROL,
            "alt" => result |= KeyModifiers::ALT,
            "shift" => result |= KeyModifiers::SHIFT,
            _ => {}
        }
    }
    result
}

/// Load keybindings from `~/.strands/keybindings.json`, falling back to defaults.
pub fn load_keybindings() -> KeybindingMap {
    let mut map = default_keybindings();

    let path = dirs_path().join("keybindings.json");
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(file) = serde_json::from_str::<KeybindingFile>(&content) {
            for entry in file.bindings {
                if let Some(code) = parse_key(&entry.key) {
                    let modifiers = parse_modifiers(&entry.modifiers);
                    map.entry(entry.action)
                        .or_default()
                        .push(KeyChord::new(modifiers, code));
                }
            }
        }
    }

    map
}

fn dirs_path() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        std::path::PathBuf::from(home).join(".strands")
    } else {
        std::path::PathBuf::from(".strands")
    }
}
