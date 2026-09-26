//! Picking a terminal's answer to the OSC 11 background query back out of
//! the dashboard's key events.
//!
//! crossterm has no notion of an OSC reply. It reads the terminal's answer
//! (`ESC ] 11 ; rgb:ffff/ffff/ffff BEL`) off stdin like anything else typed,
//! and parses it as keys: `ESC ]` becomes Alt+`]`, each character of the body
//! its own key, and the terminator either Ctrl+G (BEL is `0x07`) or Alt+`\`
//! (ST is `ESC \`). Left alone, those keys would reach [`super::App::on_key`]
//! and the refresh check — an `r` from `rgb` alone is a refetch. So while a
//! query is outstanding, every key passes through a [`ReplyReader`] first,
//! which swallows exactly the keys that spell a reply and hands everything
//! else on untouched.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::theme::{self, Background};

/// The reply's first characters after `ESC ]`. A capture that has not seen
/// all of these yet may still be a user's own Alt+`]` followed by ordinary
/// keys, and is given back rather than swallowed the moment it diverges.
const REPLY_PREFIX: &[u8] = b"11;";

/// Longer than any real reply (`11;rgba:ffff/ffff/ffff/ffff` is 27 bytes).
/// A capture past this is not a reply this can read: its keys are still
/// swallowed up to its terminator, since they are the terminal's and not the
/// user's, but no longer stored.
const MAX_REPLY_BODY: usize = 64;

/// What feeding one key to a [`ReplyReader`] produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Fed {
    /// Keys that were not part of a reply, in the order they were pressed,
    /// for the event loop to handle as it would have without a reader. Empty
    /// while a capture is holding them; several at once when a capture turns
    /// out not to be a reply after all and gives back what it held.
    pub(super) keys: Vec<KeyEvent>,
    /// The background a complete reply named. `None` both while no reply has
    /// finished and when one finished that could not be read — the second is
    /// "cannot be told," which leaves the theme alone just like silence does.
    pub(super) background: Option<Background>,
}

impl Fed {
    fn pass(keys: Vec<KeyEvent>) -> Self {
        Self {
            keys,
            background: None,
        }
    }
}

/// Recognises one reply to [`theme::BACKGROUND_QUERY`] among key events.
///
/// Pure state over keys, fed one at a time, so it is tested exactly like
/// `App::on_key` is: by handing it the keys crossterm would, with no terminal
/// anywhere near it.
#[derive(Debug, Default)]
pub(super) struct ReplyReader {
    /// The keys held since an Alt+`]`, and the bytes they stand for. `None`
    /// when not capturing.
    capture: Option<Capture>,
    /// Set once a reply has been read, readable or not. The query is sent
    /// once, so there is nothing left to wait for, and every key after it —
    /// a user's own Alt+`]` included — passes straight through.
    answered: bool,
}

#[derive(Debug, Default)]
struct Capture {
    held: Vec<KeyEvent>,
    body: Vec<u8>,
}

impl ReplyReader {
    /// Feed the next key read from the terminal.
    pub(super) fn feed(&mut self, key: KeyEvent) -> Fed {
        if self.answered || key.kind == KeyEventKind::Release {
            return Fed::pass(vec![key]);
        }

        let Some(mut capture) = self.capture.take() else {
            if is_alt(key, ']') {
                self.capture = Some(Capture {
                    held: vec![key],
                    ..Capture::default()
                });
                return Fed::default();
            }
            return Fed::pass(vec![key]);
        };

        let terminator: Option<&[u8]> = if is_bel(key) {
            Some(b"\x07")
        } else if is_alt(key, '\\') {
            Some(b"\x1b\\")
        } else {
            None
        };
        if let Some(terminator) = terminator {
            if !capture.body.starts_with(REPLY_PREFIX) {
                // Too short to have been a reply: the user's own keys.
                capture.held.push(key);
                return Fed::pass(capture.held);
            }
            // An overlong reply's `body` is only its first
            // `MAX_REPLY_BODY` bytes, more than any colour this reads, so it
            // parses as `None` rather than as a colour it never named.
            self.answered = true;
            let mut reply = b"\x1b]".to_vec();
            reply.extend_from_slice(&capture.body);
            reply.extend_from_slice(terminator);
            return Fed {
                keys: Vec::new(),
                background: theme::parse_background_reply(&reply),
            };
        }

        let Some(byte) = reply_byte(key) else {
            return self.not_a_reply(capture, key);
        };
        if capture.body.len() >= MAX_REPLY_BODY {
            // Only ever reached past a confirmed `11;` (see below), so this
            // is the terminal still talking: swallowed, not kept.
            self.capture = Some(capture);
            return Fed::default();
        }
        capture.body.push(byte);
        capture.held.push(key);

        let confirmed = capture.body.len().min(REPLY_PREFIX.len());
        if capture.body[..confirmed] != REPLY_PREFIX[..confirmed] {
            // Diverged from `11;` before it was even established: an Alt+`]`
            // the user pressed, and whatever they typed after it. Given back
            // whole, so none of it is lost.
            return Fed::pass(capture.held);
        }
        self.capture = Some(capture);
        Fed::default()
    }

    /// A key no reply contains arrived mid-capture.
    ///
    /// Before `11;` is confirmed, everything held was the user's and goes
    /// back with this key. After it, the held keys were a reply's that broke
    /// off — dropped, since replaying `11;rgb:…` as keystrokes is exactly the
    /// damage this reader exists to prevent — and this key, which a reply
    /// never contains, is the user's.
    fn not_a_reply(&mut self, mut capture: Capture, key: KeyEvent) -> Fed {
        if capture.body.starts_with(REPLY_PREFIX) {
            self.answered = true;
            return Fed::pass(vec![key]);
        }
        capture.held.push(key);
        Fed::pass(capture.held)
    }
}

fn is_alt(key: KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c) && key.modifiers == KeyModifiers::ALT
}

/// BEL, `0x07`, which crossterm reads as Ctrl+G.
fn is_bel(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('g') && key.modifiers == KeyModifiers::CONTROL
}

/// The byte a reply's body character arrived as, or `None` for a key no
/// reply body contains. Printable ASCII only; crossterm marks an uppercase
/// hex digit with SHIFT, so SHIFT alone is allowed through too.
fn reply_byte(key: KeyEvent) -> Option<u8> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    if !(key.modifiers - KeyModifiers::SHIFT).is_empty() || !c.is_ascii_graphic() {
        return None;
    }
    u8::try_from(c).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The keys crossterm 0.29's parser makes of `bytes`, for the subset of
    /// bytes a reply is made of: `ESC x` is Alt+`x`, `0x07` is Ctrl+G,
    /// anything else printable is itself (uppercase with SHIFT). Written out
    /// here rather than calling crossterm's parser, which is private — so a
    /// test reads the same keys the dashboard would, from the same bytes a
    /// fixture reply is written in.
    fn keys(bytes: &[u8]) -> Vec<KeyEvent> {
        let mut out = Vec::new();
        let mut iter = bytes.iter().copied();
        while let Some(byte) = iter.next() {
            let key = match byte {
                0x1b => {
                    let next = iter.next().expect("ESC is always followed in a fixture");
                    KeyEvent::new(KeyCode::Char(char::from(next)), KeyModifiers::ALT)
                }
                0x07 => KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
                b if b.is_ascii_uppercase() => {
                    KeyEvent::new(KeyCode::Char(char::from(b)), KeyModifiers::SHIFT)
                }
                b => KeyEvent::new(KeyCode::Char(char::from(b)), KeyModifiers::NONE),
            };
            out.push(key);
        }
        out
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// Feed every key, collecting what passed through and the last
    /// background any of them named.
    fn feed_all(reader: &mut ReplyReader, keys: &[KeyEvent]) -> Fed {
        let mut total = Fed::default();
        for &key in keys {
            let fed = reader.feed(key);
            total.keys.extend(fed.keys);
            total.background = fed.background.or(total.background);
        }
        total
    }

    #[test]
    fn a_white_reply_reads_as_light_and_swallows_every_key_it_arrived_as() {
        let mut reader = ReplyReader::default();
        let fed = feed_all(&mut reader, &keys(b"\x1b]11;rgb:ffff/ffff/ffff\x07"));
        assert_eq!(fed.keys, Vec::new());
        assert_eq!(fed.background, Some(Background::Light));
    }

    #[test]
    fn a_black_reply_terminated_by_st_reads_as_dark() {
        let mut reader = ReplyReader::default();
        let fed = feed_all(&mut reader, &keys(b"\x1b]11;rgb:0000/0000/0000\x1b\\"));
        assert_eq!(fed.keys, Vec::new());
        assert_eq!(fed.background, Some(Background::Dark));
    }

    #[test]
    fn uppercase_hex_arrives_shifted_and_still_reads() {
        let mut reader = ReplyReader::default();
        let fed = feed_all(&mut reader, &keys(b"\x1b]11;rgb:FFFF/FFFF/FFFF\x07"));
        assert_eq!(fed.keys, Vec::new());
        assert_eq!(fed.background, Some(Background::Light));
    }

    #[test]
    fn keys_before_and_after_a_reply_pass_through_in_order() {
        let mut reader = ReplyReader::default();
        let mut input = vec![press('j')];
        input.extend(keys(b"\x1b]11;rgb:ffff/ffff/ffff\x07"));
        input.push(press('k'));
        let fed = feed_all(&mut reader, &input);
        assert_eq!(fed.keys, vec![press('j'), press('k')]);
        assert_eq!(fed.background, Some(Background::Light));
    }

    #[test]
    fn an_unreadable_reply_is_still_swallowed_and_names_no_background() {
        let mut reader = ReplyReader::default();
        let fed = feed_all(&mut reader, &keys(b"\x1b]11;cmyk:1/2/3/4\x07"));
        assert_eq!(fed.keys, Vec::new());
        assert_eq!(fed.background, None);
    }

    #[test]
    fn a_users_own_alt_bracket_and_what_follows_it_are_given_back() {
        let mut reader = ReplyReader::default();
        let alt_bracket = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT);
        let fed = feed_all(&mut reader, &[alt_bracket, press('j')]);
        assert_eq!(fed.keys, vec![alt_bracket, press('j')]);
        assert_eq!(fed.background, None);
    }

    #[test]
    fn a_capture_that_diverges_after_matching_part_of_the_prefix_gives_it_all_back() {
        let mut reader = ReplyReader::default();
        let alt_bracket = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT);
        let fed = feed_all(&mut reader, &[alt_bracket, press('1'), press('q')]);
        assert_eq!(fed.keys, vec![alt_bracket, press('1'), press('q')]);
    }

    #[test]
    fn a_non_character_key_before_the_prefix_is_confirmed_gives_everything_back() {
        let mut reader = ReplyReader::default();
        let alt_bracket = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let fed = feed_all(&mut reader, &[alt_bracket, down]);
        assert_eq!(fed.keys, vec![alt_bracket, down]);
    }

    #[test]
    fn a_terminator_before_the_prefix_is_confirmed_gives_everything_back() {
        let mut reader = ReplyReader::default();
        let alt_bracket = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT);
        let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        let fed = feed_all(&mut reader, &[alt_bracket, ctrl_g]);
        assert_eq!(fed.keys, vec![alt_bracket, ctrl_g]);
        assert_eq!(fed.background, None);
    }

    #[test]
    fn a_reply_broken_off_by_another_key_drops_the_reply_and_keeps_the_key() {
        let mut reader = ReplyReader::default();
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut input = keys(b"\x1b]11;rgb:ff");
        input.push(down);
        let fed = feed_all(&mut reader, &input);
        assert_eq!(fed.keys, vec![down]);
        assert_eq!(fed.background, None);
    }

    #[test]
    fn an_overlong_reply_is_swallowed_to_its_end_without_being_stored() {
        let mut reader = ReplyReader::default();
        let mut input = keys(b"\x1b]11;rgb:");
        input.extend(std::iter::repeat_n(press('f'), MAX_REPLY_BODY * 4));
        input.extend(keys(b"\x07"));
        let fed = feed_all(&mut reader, &input);
        assert_eq!(fed.keys, Vec::new());
        assert_eq!(fed.background, None);
        // And the reader is done: the next key is the user's.
        assert_eq!(reader.feed(press('j')).keys, vec![press('j')]);
    }

    #[test]
    fn an_overlong_reply_broken_off_by_another_key_keeps_the_key() {
        let mut reader = ReplyReader::default();
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut input = keys(b"\x1b]11;");
        input.extend(std::iter::repeat_n(press('f'), MAX_REPLY_BODY * 2));
        input.push(down);
        let fed = feed_all(&mut reader, &input);
        assert_eq!(fed.keys, vec![down]);
    }

    #[test]
    fn once_answered_every_key_passes_through_even_an_alt_bracket() {
        let mut reader = ReplyReader::default();
        feed_all(&mut reader, &keys(b"\x1b]11;rgb:ffff/ffff/ffff\x07"));
        let again = feed_all(&mut reader, &keys(b"\x1b]11;rgb:0/0/0\x07"));
        assert_eq!(again.keys.len(), keys(b"\x1b]11;rgb:0/0/0\x07").len());
        assert_eq!(again.background, None);
    }

    #[test]
    fn a_key_release_is_never_captured() {
        let mut reader = ReplyReader::default();
        let mut release = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT);
        release.kind = KeyEventKind::Release;
        assert_eq!(reader.feed(release).keys, vec![release]);
        assert_eq!(reader.feed(press('j')).keys, vec![press('j')]);
    }
}
