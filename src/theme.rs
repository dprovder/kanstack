//! Visual tokens.
//!
//! The look borrows from card-board UIs rather than from `git log`: generous vertical
//! rhythm, a muted id above a bright title, and metadata that recedes. Cards are
//! separated by whitespace instead of boxes — terminal box-drawing at card density reads
//! as noise, and the whole point is to feel less like a graph dump.

use ratatui::style::{Color, Modifier, Style};

use crate::board::Tone;

pub const TITLE: Color = Color::Indexed(253);
pub const MUTED: Color = Color::Indexed(244);
pub const FAINT: Color = Color::Indexed(240);
pub const ID: Color = Color::Indexed(245);

pub const GOOD: Color = Color::Indexed(114);
pub const WARN: Color = Color::Indexed(179);
pub const BAD: Color = Color::Indexed(203);
pub const ACCENT: Color = Color::Indexed(110);

/// Background for the card under the cursor.
pub const SELECTED_BG: Color = Color::Indexed(236);
/// Background for the card being moved.
pub const PICKED_BG: Color = Color::Indexed(238);

pub fn title(selected: bool) -> Style {
    let s = Style::default().fg(TITLE);
    if selected {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn faint() -> Style {
    Style::default().fg(FAINT)
}

pub fn id() -> Style {
    Style::default().fg(ID)
}

pub fn tone(t: Tone) -> Style {
    Style::default().fg(match t {
        Tone::Neutral => FAINT,
        Tone::Good => GOOD,
        Tone::Warn => WARN,
        Tone::Bad => BAD,
        Tone::Accent => ACCENT,
    })
}

/// Lane header dot. Takes the same [`LaneState`](crate::board::LaneState) the status badge
/// prints, so the colour and the word can never disagree — they did once, and an empty
/// lane came out the same green as a fully pushed one.
pub fn status_dot(state: Option<crate::board::LaneState>) -> Style {
    match state {
        None => Style::default().fg(FAINT),
        Some(s) => tone(s.tone()),
    }
}
