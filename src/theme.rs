//! Visual tokens.
//!
//! The look borrows from card-board UIs rather than from `git log`: generous vertical
//! rhythm, a muted id above a bright title, and metadata that recedes. Cards are
//! separated by whitespace instead of boxes — terminal box-drawing at card density reads
//! as noise, and the whole point is to feel less like a graph dump.
//!
//! Grey text (title/muted/faint/id) never names a color at all — it leaves `fg` unset,
//! so the terminal's own default foreground shows through, and reaches for `Modifier::DIM`
//! /`BOLD` for the tiers below/above it. A named ANSI grey (`Color::Gray`,
//! `Color::DarkGray`) is still a *separate* palette slot from "default foreground": themes
//! tune it for things like comments or line numbers, not body text, so it can end up too
//! pale to read against that same theme's own background (as happened here — a light
//! Ghostty theme with washed-out ANSI greys). Only the tone accents (good/warn/bad/accent)
//! still name a color, since their whole job is to *be* a specific hue.

use ratatui::style::{Color, Modifier, Style};

use crate::board::Tone;

pub const GOOD: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const BAD: Color = Color::Red;
pub const ACCENT: Color = Color::Cyan;

pub fn title(selected: bool) -> Style {
    let s = Style::default();
    if selected {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

pub fn muted() -> Style {
    Style::default()
}

pub fn faint() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub fn id() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn tone(t: Tone) -> Style {
    match t {
        Tone::Neutral => faint(),
        Tone::Good => Style::default().fg(GOOD),
        Tone::Warn => Style::default().fg(WARN),
        Tone::Bad => Style::default().fg(BAD),
        Tone::Accent => Style::default().fg(ACCENT),
    }
}

/// Highlight for the card/row under the cursor. Reverse video instead of a fixed
/// background color: it swaps whatever foreground/background are already in play, so it
/// reads correctly no matter which way the terminal's theme is set.
pub fn selected_bg() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Highlight for the card being moved. Same reverse-video trick as `selected_bg`, plus
/// bold so the two remain distinguishable from each other, not just from an unselected row.
pub fn picked_bg() -> Style {
    Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
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
