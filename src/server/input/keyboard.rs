//! How much the client's terminal says about each key (kitty keyboard protocol).
//!
//! The client asks its terminal for disambiguated keys with their alternates (flags 1 and 4):
//! typed text still arrives as text, and a key with Ctrl or Alt says where it is on the keyboard.
//! While the keys after the prefix are awaited, the server raises that to every key as a
//! sequence with the text it types (flags 8 and 16 too), so that a key of the digit row or of a
//! non-Latin layout also says where it is (see [`crate::input::Key::by_place`]). It sets the
//! client's own entry of the terminal's stack (`CSI = flags ; 1 u`) and never pushes one: the
//! client's single pop when it goes gives the terminal back as it was. A terminal that cannot
//! send the typed text along is left as the client set it.

use crate::server::ui::Mode;
use crate::server::{ClientId, Server, frame_bytes};

/// Every key as a sequence, with its alternates and its text (1 + 4 + 8 + 16).
const KEYS_BY_PLACE: &[u8] = b"\x1b[=29;1u";
/// What the terminal granted, asked once: the answer is `CSI ? flags u`.
const KEYS_ASK: &[u8] = b"\x1b[?u";
/// Back to what the client asks for (1 + 4).
const KEYS_AS_TEXT: &[u8] = b"\x1b[=5;1u";
/// The flags that make keys say where they are without losing what they type.
const BY_PLACE: u32 = 4 | 8 | 16;

impl Server {
    /// Raise each client's terminal while it is in the leader menu, lower it otherwise.
    pub(in crate::server) fn follow_keyboard(&mut self) {
        for c in self.clients.values_mut().filter(|c| c.attached) {
            let wants = matches!(c.mode, Mode::Leader { .. }) && c.keys_full != Some(false);
            if c.keys_by_place == wants {
                continue;
            }
            let mut bytes = if wants { KEYS_BY_PLACE.to_vec() } else { KEYS_AS_TEXT.to_vec() };
            if wants && c.keys_full.is_none() {
                bytes.extend_from_slice(KEYS_ASK);
            }
            // Recorded once on its way only: a full queue means trying again next time.
            if c.out.try_send(frame_bytes(&bytes)).is_ok() {
                c.keys_by_place = wants;
            }
        }
    }

    /// The terminal's answer to [`KEYS_ASK`]: whether it sends keys' places with their text.
    /// It goes to no pane.
    pub(super) fn keyboard_reply(&mut self, id: ClientId, raw: &[u8]) -> bool {
        let Some(flags) = raw
            .strip_prefix(b"\x1b[?")
            .and_then(|r| r.strip_suffix(b"u"))
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| d.parse::<u32>().ok())
        else {
            return false;
        };
        if let Some(c) = self.clients.get_mut(&id) {
            c.keys_full = Some(flags & BY_PLACE == BY_PLACE);
        }
        self.follow_keyboard();
        true
    }
}
