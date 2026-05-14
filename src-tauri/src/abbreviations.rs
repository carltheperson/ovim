//! Text abbreviation expansion for insert-mode typing.
//!
//! Abbreviations are intentionally explicit: these are short tokens the user
//! would not normally type as standalone words.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::keyboard::{self, KeyCode, KeyEvent};
use crate::nvim_edit::vim_eligibility::{self, FocusCheckReason};

macro_rules! define_abbreviations {
    ($($typed:literal => $expanded:literal),* $(,)?) => {
        const ABBREVIATIONS: &[Abbreviation] = &[
            $(
                Abbreviation {
                    typed: $typed,
                    expanded: $expanded,
                },
            )*
        ];
    };
}

#[derive(Debug, Clone, Copy)]
struct Abbreviation {
    typed: &'static str,
    expanded: &'static str,
}

define_abbreviations! {
    "t" => "the",
    "ab" => "about",
    "h" => "how",
    "th" => "that",
    "te" => "there",
    "its" => "it's",
    "im" => "I'm",
    "wer" => "we're",
    "bc" => "because",
    "bf" => "before",
    "af" => "after",
    "sm" => "some",
    "ar" => "are",
    "u" => "you",
    "ha" => "have",
    "st" => "something",
    "sh" => "should",
    "ma" => "maybe",
    "i" => "I",
    "w" => "what",
    "wh" => "where",
    "d" => "doing",
}

#[derive(Debug)]
pub struct AbbreviationState {
    current_word: String,
    current_word_started_after_boundary: bool,
    next_word_starts_after_boundary: bool,
}

impl Default for AbbreviationState {
    fn default() -> Self {
        Self {
            current_word: String::new(),
            current_word_started_after_boundary: false,
            next_word_starts_after_boundary: true,
        }
    }
}

#[derive(Debug, Clone)]
struct Expansion {
    abbreviation_len: usize,
    replacement: String,
    boundary: Boundary,
}

#[derive(Debug, Clone, Copy)]
enum Boundary {
    Space,
    Return,
    Punctuation { keycode: KeyCode, shift: bool },
}

impl AbbreviationState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.current_word.clear();
        self.current_word_started_after_boundary = false;
        self.next_word_starts_after_boundary = true;
    }

    fn clear_current_word_without_boundary(&mut self) {
        self.current_word.clear();
        self.current_word_started_after_boundary = false;
        self.next_word_starts_after_boundary = false;
    }

    pub fn process_key(&mut self, event: &KeyEvent) -> bool {
        if !event.is_key_down {
            return false;
        }

        if has_control_modifiers(event) {
            self.clear_current_word_without_boundary();
            return false;
        }

        let keycode = match event.keycode() {
            Some(keycode) => keycode,
            None => {
                self.clear_current_word_without_boundary();
                return false;
            }
        };

        if let Some(letter) = event_letter(keycode, event) {
            if self.current_word.is_empty() {
                self.current_word_started_after_boundary = self.next_word_starts_after_boundary;
            }
            self.next_word_starts_after_boundary = false;
            self.current_word.push(letter);
            if trim_current_word(&mut self.current_word) {
                self.current_word_started_after_boundary = false;
            }
            return false;
        }

        if keycode == KeyCode::Quote && !event.modifiers.shift {
            if self.current_word.is_empty() {
                self.current_word_started_after_boundary = self.next_word_starts_after_boundary;
            }
            self.next_word_starts_after_boundary = false;
            self.current_word.push('\'');
            if trim_current_word(&mut self.current_word) {
                self.current_word_started_after_boundary = false;
            }
            return false;
        }

        let Some(boundary) = boundary_from_key(keycode, event) else {
            if !self.current_word.is_empty() {
                self.clear_current_word_without_boundary();
            }
            return false;
        };

        let expansion = self.pending_expansion(boundary);
        self.current_word.clear();
        self.current_word_started_after_boundary = false;
        self.next_word_starts_after_boundary = true;

        if let Some(expansion) = expansion {
            if !vim_eligibility::allows_vim_at_decision_point(
                FocusCheckReason::AbbreviationExpansionCheck,
            ) {
                return false;
            }
            execute_expansion_async(expansion);
            true
        } else {
            false
        }
    }

    fn pending_expansion(&self, boundary: Boundary) -> Option<Expansion> {
        let abbreviation = ABBREVIATIONS
            .iter()
            .find(|entry| entry.typed == self.current_word.to_lowercase())?;

        if !self.current_word_started_after_boundary {
            return None;
        }

        Some(Expansion {
            abbreviation_len: self.current_word.chars().count(),
            replacement: replacement_for_typed_word(&self.current_word, abbreviation.expanded),
            boundary,
        })
    }
}

pub type SharedAbbreviationState = Arc<Mutex<AbbreviationState>>;

pub fn create_abbreviation_state() -> SharedAbbreviationState {
    Arc::new(Mutex::new(AbbreviationState::new()))
}

fn execute_expansion_async(expansion: Expansion) {
    thread::spawn(move || {
        thread::sleep(Duration::from_micros(500));
        if let Err(e) = execute_expansion(expansion) {
            log::error!("Failed to expand abbreviation: {}", e);
        }
    });
}

fn execute_expansion(expansion: Expansion) -> Result<(), String> {
    for _ in 0..expansion.backspace_count() {
        keyboard::backspace()?;
    }

    keyboard::type_text(&expansion.replacement)?;
    type_boundary(expansion.boundary)
}

impl Expansion {
    fn backspace_count(&self) -> usize {
        self.abbreviation_len + self.boundary.backspace_count()
    }
}

impl Boundary {
    fn backspace_count(self) -> usize {
        1
    }
}

fn type_boundary(boundary: Boundary) -> Result<(), String> {
    match boundary {
        Boundary::Space => keyboard::type_text(" "),
        Boundary::Return => keyboard::inject_return(),
        Boundary::Punctuation { keycode, shift } => keyboard::type_char(keycode, shift),
    }
}

fn has_control_modifiers(event: &KeyEvent) -> bool {
    event.modifiers.control || event.modifiers.option || event.modifiers.command
}

fn event_letter(keycode: KeyCode, event: &KeyEvent) -> Option<char> {
    let char = keycode.to_char()?;
    if !char.is_ascii_alphabetic() {
        return None;
    }

    let uppercase = event.modifiers.shift ^ event.modifiers.caps_lock;
    Some(if uppercase {
        char.to_ascii_uppercase()
    } else {
        char
    })
}

fn boundary_from_key(keycode: KeyCode, event: &KeyEvent) -> Option<Boundary> {
    match keycode {
        KeyCode::Space if !event.modifiers.shift => Some(Boundary::Space),
        KeyCode::Return if !event.modifiers.shift => Some(Boundary::Return),
        KeyCode::Period
        | KeyCode::Comma
        | KeyCode::Semicolon
        | KeyCode::Slash
        | KeyCode::Minus
        | KeyCode::RightBracket => Some(Boundary::Punctuation {
            keycode,
            shift: event.modifiers.shift,
        }),
        KeyCode::Num1 if event.modifiers.shift => Some(Boundary::Punctuation {
            keycode,
            shift: true,
        }),
        _ => None,
    }
}

fn replacement_for_typed_word(typed: &str, expanded: &str) -> String {
    if !typed.chars().next().is_some_and(char::is_uppercase) {
        return expanded.to_string();
    }

    let mut chars = expanded.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn trim_current_word(current_word: &mut String) -> bool {
    let max_len = ABBREVIATIONS
        .iter()
        .map(|entry| entry.typed.len())
        .max()
        .unwrap_or(0);

    if current_word.len() <= max_len {
        return false;
    }

    let keep_from = current_word
        .char_indices()
        .nth(current_word.chars().count().saturating_sub(max_len))
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    current_word.drain(..keep_from);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capitalizes_replacement_when_first_abbreviation_letter_is_uppercase() {
        assert_eq!(replacement_for_typed_word("Th", "that"), "That");
        assert_eq!(replacement_for_typed_word("Its", "it's"), "It's");
    }

    #[test]
    fn keeps_lowercase_replacement_for_lowercase_abbreviation() {
        assert_eq!(replacement_for_typed_word("th", "that"), "that");
        assert_eq!(replacement_for_typed_word("its", "it's"), "it's");
    }

    #[test]
    fn apostrophe_keeps_contraction_as_one_word() {
        let mut state = AbbreviationState::new();
        assert!(!state.process_key(&key_down_for_keycode(KeyCode::Space)));

        for char in ['d', 'i', 'd', 'n'] {
            let event = key_down_for_char(char);
            assert!(!state.process_key(&event));
        }

        assert!(!state.process_key(&KeyEvent {
            code: KeyCode::Quote.as_raw(),
            modifiers: Default::default(),
            is_key_down: true,
        }));
        assert!(!state.process_key(&key_down_for_char('t')));

        assert!(state.pending_expansion(Boundary::Space).is_none());
    }

    #[test]
    fn unknown_punctuation_prevents_single_letter_mid_word_expansion() {
        let mut state = AbbreviationState::new();
        assert!(!state.process_key(&key_down_for_keycode(KeyCode::Space)));

        for char in ['d', 'i', 'd', 'n'] {
            assert!(!state.process_key(&key_down_for_char(char)));
        }

        assert!(!state.process_key(&key_down_for_keycode(KeyCode::Grave)));
        assert!(!state.process_key(&key_down_for_char('t')));

        assert!(state.pending_expansion(Boundary::Space).is_none());
    }

    #[test]
    fn expands_single_letter_after_known_boundary() {
        let mut state = AbbreviationState::new();
        assert!(!state.process_key(&key_down_for_keycode(KeyCode::Space)));
        assert!(!state.process_key(&key_down_for_char('t')));

        assert!(state.pending_expansion(Boundary::Space).is_some());
    }

    #[test]
    fn expands_first_word_from_fresh_state() {
        let mut state = AbbreviationState::new();
        assert!(!state.process_key(&key_down_for_char('t')));

        assert!(state.pending_expansion(Boundary::Space).is_some());
    }

    #[test]
    fn ignores_unknown_noise_before_fresh_word() {
        let mut state = AbbreviationState::new();
        assert!(!state.process_key(&key_down_for_keycode(KeyCode::Grave)));
        assert!(!state.process_key(&key_down_for_char('t')));

        assert!(state.pending_expansion(Boundary::Space).is_some());
    }

    fn key_down_for_char(char: char) -> KeyEvent {
        let keycode = match char {
            'd' => KeyCode::D,
            'i' => KeyCode::I,
            'n' => KeyCode::N,
            't' => KeyCode::T,
            _ => unreachable!("test only maps the chars it uses"),
        };

        key_down_for_keycode(keycode)
    }

    fn key_down_for_keycode(keycode: KeyCode) -> KeyEvent {
        KeyEvent {
            code: keycode.as_raw(),
            modifiers: Default::default(),
            is_key_down: true,
        }
    }
}
