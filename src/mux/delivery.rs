//! Typing a message into a harness's pane and confirming it was submitted, instead of
//! assuming so.
//!
//! A send has two steps that can each fail silently: the text has to reach the harness's
//! input box, and Enter has to submit it from there. A send that reports success when either
//! step failed is worse than one that reports an error, because the caller goes on believing
//! the lane was told something it never saw. So [`deliver`] looks at the pane after each
//! step, through [`Screen::read`](crate::mux::Screen::read) and the harness's own [`Harness::read_prompt`], and only
//! returns [`Delivery::Confirmed`] after seeing both: the message in the box, then the box
//! no longer holding it.
//!
//! **Arrival** means the box's visible characters (whitespace and control characters
//! dropped) end with the message's last [`TAIL_CHARS`] visible characters, or the box holds
//! a collapsed paste. Dropping whitespace makes the match indifferent to where the harness
//! wraps a line and how it indents continuation rows. Matching the *end* survives a box too
//! short for the whole message, because input that overflows the box scrolls to keep its end
//! in view. A long paste the harness folds into a placeholder hides the text entirely, which
//! is why a collapsed paste counts on its own. And a message that hasn't landed yet can't
//! match, which is what the stillness check this replaced got wrong in both directions: a
//! busy Claude Code pane redraws its spinner and timer constantly, so it never looked still
//! and Enter went out blind after a timeout, while an idle pane looked still *before* the
//! paste had drawn anything.
//!
//! **Acceptance** means the box is still on screen and no longer holds the message. That
//! covers both outcomes that count as delivered: submitted, or queued behind a busy turn
//! (Claude Code moves a queued message out of the box and delivers it when the turn ends).
//! A box that still holds the message after a wait gets Enter again, with longer waits each
//! time. Enter is only pressed again on that positive evidence. If the box disappears
//! instead, a dialog may have taken its place, and another Enter could answer it.
//!
//! When a step can't be confirmed, [`deliver`] returns an error that says how far the send
//! got: nothing typed, typed but never seen, or still sitting in the box.
//!
//! What can't be checked at all is reported as [`Delivery::Unconfirmed`], with the reason,
//! and sent the old way through [`Multiplexer::type_line`]. That happens for a backend with
//! no [`Multiplexer::screen`] (Ghostty, Orca), a harness with no `read_prompt` (all but
//! Claude Code, today), or a pane whose harness isn't known.

use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::harness::{Harness, Prompt};
use crate::mux::Multiplexer;

/// How many of a message's last visible characters must end the input box for the message to
/// count as arrived. Long enough that placeholder or suggestion text in an empty box won't
/// match by accident, and short enough to fit in the visible part of a box that has scrolled.
pub const TAIL_CHARS: usize = 40;

/// How a send turned out, when it didn't fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// The message was seen in the harness's input box, then seen to leave it after Enter.
    /// It was either submitted or queued behind a busy turn.
    Confirmed,
    /// Typed and submitted, but neither step could be checked. Holds the reason.
    Unconfirmed(String),
}

/// How long [`deliver`] waits at each step. Tests shorten these.
#[derive(Debug, Clone)]
pub struct Timing {
    /// The gap between screen reads.
    pub poll: Duration,
    /// How long to wait for the input box before typing, and then for the message to show up
    /// in it.
    pub arrival: Duration,
    /// How long to watch for the box to clear after each press of Enter. One press per entry,
    /// so its length is the most times Enter is pressed.
    pub enter_waits: Vec<Duration>,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            poll: Duration::from_millis(100),
            arrival: Duration::from_secs(5),
            enter_waits: vec![Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)],
        }
    }
}

/// What the input box shows, relative to the message being sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// The box is on screen and holds the message.
    Holding,
    /// The box is on screen and doesn't hold the message.
    Clear,
    /// The box isn't on screen.
    Hidden,
}

/// Types `text` into `pane`, presses Enter, and confirms both steps took, as described in the
/// module doc. `harness` is what runs in the pane, or `None` if that isn't known.
pub fn deliver(mux: &dyn Multiplexer, pane: &str, text: &str, harness: Option<&dyn Harness>, timing: &Timing) -> Result<Delivery> {
    let Some(screen) = mux.screen() else {
        mux.type_line(pane, text)?;
        return Ok(Delivery::Unconfirmed(format!("{} can't read a pane back", mux.name())));
    };
    let Some(harness) = harness else {
        mux.type_line(pane, text)?;
        return Ok(Delivery::Unconfirmed("which harness runs in this pane isn't known".to_string()));
    };
    if visible(text).is_empty() {
        mux.type_line(pane, text)?;
        return Ok(Delivery::Unconfirmed("the message has no visible text to look for".to_string()));
    }
    let id = harness.id();
    let tail = tail(text);
    let look = || -> Option<Option<Seen>> {
        let shown = screen.read(pane).ok()?;
        Some(harness.read_prompt(&shown).map(|prompt| seen(&prompt, &tail)))
    };

    // Before typing anything, wait for the box: right after a spawn the harness may not have
    // drawn it yet, and typing into a dialog could answer it.
    let started = Instant::now();
    let mut last = None;
    loop {
        match look() {
            Some(None) => {
                mux.type_line(pane, text)?;
                return Ok(Delivery::Unconfirmed(format!("kanstack can't read {id}'s input box")));
            }
            Some(Some(Seen::Hidden)) => last = Some(Seen::Hidden),
            Some(Some(_)) => break,
            None => {}
        }
        if started.elapsed() >= timing.arrival {
            match last {
                Some(_) => bail!(
                    "{id}'s input box isn't on screen (a permission prompt or other dialog may be open, or it hasn't started), so nothing was typed"
                ),
                None => bail!("couldn't read the pane back, so nothing was typed"),
            }
        }
        sleep(timing.poll);
    }

    screen.paste(pane, text)?;
    let pasted = Instant::now();
    while look() != Some(Some(Seen::Holding)) {
        if pasted.elapsed() >= timing.arrival {
            bail!(
                "the message never showed up in {id}'s input box within {}, so Enter was not pressed: it was not delivered, and may be sitting partly typed in the box",
                secs(timing.arrival)
            );
        }
        sleep(timing.poll);
    }

    for wait in &timing.enter_waits {
        screen.press_enter(pane)?;
        let pressed = Instant::now();
        let mut last = None;
        while pressed.elapsed() < *wait {
            sleep(timing.poll);
            match look() {
                Some(Some(Seen::Clear)) => return Ok(Delivery::Confirmed),
                Some(Some(seen)) => last = Some(seen),
                _ => {}
            }
        }
        match last {
            Some(Seen::Holding) => continue,
            Some(_) => bail!(
                "{id}'s input box disappeared after Enter and didn't come back within {}, so whether the message was submitted is unknown. Enter was not pressed again in case a dialog is now open",
                secs(*wait)
            ),
            None => bail!("couldn't read the pane back after Enter, so whether the message was submitted is unknown"),
        }
    }
    bail!(
        "the message is still sitting unsubmitted in {id}'s input box after Enter was pressed {} times: it was not delivered",
        timing.enter_waits.len()
    )
}

fn seen(prompt: &Prompt, tail: &str) -> Seen {
    match prompt {
        Prompt::Hidden => Seen::Hidden,
        Prompt::Shown { collapsed_paste: true, .. } => Seen::Holding,
        Prompt::Shown { text, .. } if visible(text).ends_with(tail) => Seen::Holding,
        Prompt::Shown { .. } => Seen::Clear,
    }
}

/// `text` without whitespace or control characters, so wrapping, indentation and anything a
/// terminal draws as a blank don't affect a comparison.
fn visible(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect()
}

/// The last [`TAIL_CHARS`] of `text`'s [`visible`] characters.
fn tail(text: &str) -> String {
    let chars: Vec<char> = visible(text).chars().collect();
    chars[chars.len().saturating_sub(TAIL_CHARS)..].iter().collect()
}

fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::harness::for_command;
    use crate::mux::pane_status::PaneStatus;
    use crate::mux::{OpenRequest, RegroupRequest, Screen};

    /// What a [`Scripted`] pane shows, given how many reads and Enters came before.
    type Script = Box<dyn Fn(usize, usize) -> Option<&'static str> + Send + Sync>;

    /// A pane whose screen comes from a script. Each call is logged, so a test can see exactly
    /// when Enter was pressed relative to what was on screen.
    struct Scripted {
        script: Script,
        reads: Mutex<usize>,
        enters: Mutex<usize>,
        log: Mutex<Vec<String>>,
        readable: bool,
    }

    impl Scripted {
        fn new(script: impl Fn(usize, usize) -> Option<&'static str> + Send + Sync + 'static) -> Scripted {
            Scripted { script: Box::new(script), reads: Mutex::default(), enters: Mutex::default(), log: Mutex::default(), readable: true }
        }

        /// One screen per read, in order, the last repeating.
        fn reads(screens: &[Option<&'static str>]) -> Scripted {
            let screens = screens.to_vec();
            Scripted::new(move |read, _| screens[read.min(screens.len() - 1)])
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn enters(&self) -> usize {
            *self.enters.lock().unwrap()
        }

        fn record(&self, line: impl Into<String>) {
            self.log.lock().unwrap().push(line.into());
        }
    }

    impl Screen for Scripted {
        fn paste(&self, _pane: &str, text: &str) -> Result<()> {
            self.record(format!("paste {text}"));
            Ok(())
        }

        fn press_enter(&self, _pane: &str) -> Result<()> {
            *self.enters.lock().unwrap() += 1;
            self.record("enter");
            Ok(())
        }

        fn read(&self, _pane: &str) -> Result<String> {
            let mut reads = self.reads.lock().unwrap();
            let shown = (self.script)(*reads, self.enters());
            *reads += 1;
            self.record("read");
            shown.map(str::to_string).ok_or_else(|| anyhow::anyhow!("read failed"))
        }
    }

    impl Multiplexer for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn running_inside(&self) -> bool {
            true
        }
        fn open_pane(&self, _req: &OpenRequest<'_>) -> Result<String> {
            unreachable!()
        }
        fn regroup(&self, _req: &RegroupRequest<'_>) -> Result<()> {
            unreachable!()
        }
        fn type_line(&self, _pane: &str, text: &str) -> Result<()> {
            self.record(format!("type_line {text}"));
            Ok(())
        }
        fn focus(&self, _pane: &str) -> Result<()> {
            unreachable!()
        }
        fn close(&self, _pane: &str) -> Result<()> {
            unreachable!()
        }
        fn probe(&self, _panes: &[&str]) -> Result<HashMap<String, PaneStatus>> {
            unreachable!()
        }
        fn screen(&self) -> Option<&dyn Screen> {
            self.readable.then_some(self as &dyn Screen)
        }
    }

    fn quick() -> Timing {
        Timing { poll: Duration::from_millis(1), arrival: Duration::from_millis(50), enter_waits: vec![Duration::from_millis(10); 3] }
    }

    fn send(pane: &Scripted, text: &str) -> Result<Delivery> {
        deliver(pane, "surface:5", text, Some(for_command("claude")), &quick())
    }

    const EMPTY: &str = "────────\n❯ \n────────\n  status";
    const HOLDING: &str = "────────\n❯ run the tests\n────────\n  status";
    const COLLAPSED: &str = "────────\n❯ [Pasted text #7 +24 lines]\n────────\n  status";
    const QUEUED: &str = "  run the tests\n  ctrl+enter to send now\n────────\n❯ Press up to edit queued messages\n────────";
    const DIALOG: &str = " Do you want to proceed?\n ❯ 1. Yes\n   2. No";

    /// The happy path: box seen, message seen in it, Enter, box seen clear. The paste waits for
    /// the box, Enter waits for the message, and success waits for the box to clear.
    #[test]
    fn a_send_is_confirmed_once_the_message_is_seen_arriving_and_then_leaving() {
        let pane = Scripted::reads(&[Some(EMPTY), Some(EMPTY), Some(HOLDING), Some(EMPTY)]);
        assert_eq!(send(&pane, "run the tests").unwrap(), Delivery::Confirmed);
        assert_eq!(pane.log(), ["read", "paste run the tests", "read", "read", "enter", "read"]);
    }

    /// A busy harness queues the message instead of starting it. That is still delivered: the
    /// message has left the box.
    #[test]
    fn a_queued_message_counts_as_delivered() {
        let pane = Scripted::reads(&[Some(EMPTY), Some(HOLDING), Some(QUEUED)]);
        assert_eq!(send(&pane, "run the tests").unwrap(), Delivery::Confirmed);
    }

    #[test]
    fn a_collapsed_paste_counts_as_arrived() {
        let pane = Scripted::reads(&[Some(EMPTY), Some(COLLAPSED), Some(EMPTY)]);
        assert_eq!(send(&pane, &"long text ".repeat(400)).unwrap(), Delivery::Confirmed);
        assert_eq!(pane.enters(), 1);
    }

    /// The bug this module exists for: Enter was pressed, and the message stayed in the box.
    /// Enter is pressed again while the box still holds it, and the send succeeds once it
    /// clears.
    #[test]
    fn enter_is_pressed_again_while_the_message_stays_in_the_box() {
        let pane = Scripted::new(|read, enters| match (read, enters) {
            (0, _) => Some(EMPTY),
            (_, 0 | 1) => Some(HOLDING),
            _ => Some(EMPTY),
        });
        assert_eq!(send(&pane, "run the tests").unwrap(), Delivery::Confirmed);
        assert_eq!(pane.enters(), 2, "{:?}", pane.log());
    }

    #[test]
    fn a_message_that_never_leaves_the_box_is_an_error_not_a_success() {
        let pane = Scripted::reads(&[Some(EMPTY), Some(HOLDING)]);
        let err = send(&pane, "run the tests").unwrap_err().to_string();
        assert!(err.contains("still sitting unsubmitted") && err.contains("3 times"), "{err}");
        assert_eq!(pane.enters(), 3, "bounded: one press per wait");
    }

    /// The stillness check this replaced pressed Enter on a pane that hadn't drawn the paste
    /// yet, because a screen that hadn't changed looked settled. Here the box never shows the
    /// message, so Enter must never be pressed.
    #[test]
    fn enter_is_never_pressed_before_the_message_is_seen() {
        let pane = Scripted::reads(&[Some(EMPTY)]);
        let err = send(&pane, "run the tests").unwrap_err().to_string();
        assert!(err.contains("never showed up") && err.contains("Enter was not pressed"), "{err}");
        assert_eq!(pane.enters(), 0, "{:?}", pane.log());
    }

    /// Placeholder or suggestion text in an empty box is on screen as plain text too. It
    /// mustn't pass for the message, which is why the match is on the message's own end.
    #[test]
    fn a_placeholder_in_the_box_is_not_the_message() {
        let suggestion = "────────\n❯ run it in the background\n────────";
        let pane = Scripted::reads(&[Some(suggestion)]);
        assert!(send(&pane, "please run the tests now").is_err());
        assert_eq!(pane.enters(), 0);
    }

    /// A wrapped long message shows only its end, broken across rows wherever the harness
    /// chose.
    #[test]
    fn arrival_ignores_wrapping_and_a_box_too_short_for_the_whole_message() {
        let text = format!("{} and finally: reply with only the word PONG-BRAVO-9.", "filler ".repeat(300));
        let wrapped = "────────\n❯ filler filler filler and finally: reply with only\n  the word PONG-BRAVO-9.\n────────";
        let pane = Scripted::reads(&[Some(EMPTY), Some(wrapped), Some(EMPTY)]);
        assert_eq!(send(&pane, &text).unwrap(), Delivery::Confirmed);
    }

    #[test]
    fn a_dialog_in_place_of_the_box_gets_nothing_typed_into_it() {
        let pane = Scripted::reads(&[Some(DIALOG)]);
        let err = send(&pane, "run the tests").unwrap_err().to_string();
        assert!(err.contains("isn't on screen") && err.contains("nothing was typed"), "{err}");
        assert!(pane.log().iter().all(|l| l == "read"), "{:?}", pane.log());
    }

    /// A box that disappears after Enter might mean a dialog opened, where another Enter
    /// could answer it. So it's an error, and Enter isn't pressed again.
    #[test]
    fn a_box_that_disappears_after_enter_is_not_pressed_again() {
        let pane = Scripted::reads(&[Some(EMPTY), Some(HOLDING), Some(DIALOG)]);
        let err = send(&pane, "run the tests").unwrap_err().to_string();
        assert!(err.contains("disappeared after Enter"), "{err}");
        assert_eq!(pane.enters(), 1);
    }

    #[test]
    fn an_unreadable_pane_gets_nothing_typed_and_says_so() {
        let pane = Scripted::reads(&[None]);
        let err = send(&pane, "run the tests").unwrap_err().to_string();
        assert!(err.contains("couldn't read the pane") && err.contains("nothing was typed"), "{err}");
    }

    /// A read that fails now and then is just no news.
    #[test]
    fn a_failed_read_along_the_way_is_retried() {
        let pane = Scripted::reads(&[None, Some(EMPTY), None, Some(HOLDING), None, Some(EMPTY)]);
        assert_eq!(send(&pane, "run the tests").unwrap(), Delivery::Confirmed);
    }

    /// What can't be checked is sent the old way and reported as unconfirmed, with the reason,
    /// never as confirmed.
    #[test]
    fn what_cant_be_checked_is_sent_unconfirmed_and_says_why() {
        let mut blind = Scripted::reads(&[Some(EMPTY)]);
        blind.readable = false;
        let delivery = send(&blind, "run the tests").unwrap();
        assert_eq!(delivery, Delivery::Unconfirmed("scripted can't read a pane back".into()));
        assert_eq!(blind.log(), ["type_line run the tests"]);

        let pane = Scripted::reads(&[Some(EMPTY)]);
        let delivery = deliver(&pane, "surface:5", "run the tests", Some(for_command("codex")), &quick()).unwrap();
        assert_eq!(delivery, Delivery::Unconfirmed("kanstack can't read codex's input box".into()));
        assert_eq!(pane.log(), ["read", "type_line run the tests"]);

        let pane = Scripted::reads(&[Some(EMPTY)]);
        let delivery = deliver(&pane, "surface:5", "run the tests", None, &quick()).unwrap();
        assert!(matches!(delivery, Delivery::Unconfirmed(_)));
        assert_eq!(pane.log(), ["type_line run the tests"]);

        let pane = Scripted::reads(&[Some(EMPTY)]);
        assert!(matches!(send(&pane, " \n").unwrap(), Delivery::Unconfirmed(_)));
    }

    #[test]
    fn the_tail_is_the_last_visible_characters() {
        assert_eq!(tail("a b\nc\td"), "abcd");
        assert_eq!(tail(&format!("{}xyz", "a".repeat(100))).len(), TAIL_CHARS);
        assert!(tail(&format!("{}xyz", "a".repeat(100))).ends_with("xyz"));
        assert_eq!(visible("esc\u{1b}[0m"), "esc[0m");
    }
}
