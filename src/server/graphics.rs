//! Images for clients: whose terminal shows them, and bringing each one up to date with the
//! images the panes show, ahead of the frames that use them.

use std::collections::HashMap;
use std::sync::Arc;

use super::clients::frame_bytes;
use super::{ClientId, Server};
use crate::graphics::{self, Picture};

/// Image data sent with one frame at most; the rest follows with the next frames.
const BUDGET: usize = 4 << 20;

#[derive(Default)]
pub(super) struct ClientImages {
    /// The terminal answered the probe (it speaks the kitty graphics protocol), and its name
    /// says it draws images over placeholder cells.
    answered: bool,
    name: Option<String>,
    capable: bool,
    /// Its cell size in pixels, once it said.
    cell: Option<(u16, u16)>,
    /// Images it has, with the generation of their placements.
    sent: HashMap<u32, u64>,
    /// The images' version when it was last brought up to date.
    version: Option<u64>,
}

impl ClientImages {
    /// What it was sent may not have arrived: send it all again.
    pub(super) fn forget(&mut self) {
        self.sent.clear();
        self.version = None;
    }
}

impl Server {
    /// Ask a client's terminal whether it shows images, and its cell size.
    pub(super) fn probe_graphics(&mut self, id: ClientId) {
        if !self.env.graphics.enabled() {
            return;
        }
        self.images.entry(id).or_default();
        if let Some(c) = self.clients.get(&id) {
            let _ = c.out.try_send(frame_bytes(graphics::probe().as_bytes()));
        }
    }

    /// Ask again for the cell size (the font may have changed with the window).
    pub(super) fn probe_cell_size(&mut self, id: ClientId) {
        if self.env.graphics.enabled()
            && let Some(c) = self.clients.get(&id)
        {
            let _ = c.out.try_send(frame_bytes(b"\x1b[16t"));
        }
    }

    /// A terminal's reply about images, its name or its cell size; false when it is about
    /// something else.
    pub(super) fn graphics_reply(&mut self, id: ClientId, reply: &str) -> bool {
        let answer = graphics::probe_answer(reply);
        let name = graphics::terminal_name(reply);
        if answer.is_some() || name.is_some() {
            // Any graphics reply is the terminal's own business: never a program's input.
            let Some(ci) = self.images.get_mut(&id) else { return true };
            if answer == Some(true) {
                ci.answered = true;
            }
            if name.is_some() {
                ci.name = name;
            }
            let capable = ci.answered && ci.name.as_deref().is_some_and(graphics::draws_placeholders);
            if capable && !ci.capable {
                ci.capable = true;
                ci.forget();
                self.update_availability();
                if let Some(c) = self.clients.get_mut(&id) {
                    c.dirty = true;
                }
            }
            return true;
        }
        if let Some((w, h)) = graphics::cell_size_answer(reply) {
            if let Some(ci) = self.images.get_mut(&id) {
                ci.cell = Some((w, h));
            }
            if self.primary.is_none_or(|p| p == id) && self.env.graphics.cell() != (w, h) {
                self.env.graphics.set_cell(w, h);
                for p in self.panes.values_mut() {
                    p.refresh_pixel_size();
                }
            }
            return true;
        }
        false
    }

    /// A client leaving: its terminal can forget the images it was sent.
    pub(super) fn graphics_farewell(&mut self, id: ClientId) {
        let Some(ci) = self.images.get_mut(&id) else { return };
        let mut out = Vec::new();
        for kid in ci.sent.keys() {
            graphics::delete(*kid, &mut out);
        }
        ci.forget();
        if let Some(c) = self.clients.get(&id)
            && !out.is_empty()
        {
            let _ = c.out.try_send(frame_bytes(&out));
        }
    }

    pub(super) fn graphics_client_gone(&mut self, id: ClientId) {
        if self.images.remove(&id).is_some() {
            self.update_availability();
        }
        // The primary client changed: its terminal's cell size counts now.
        if let Some(cell) = self.primary.and_then(|p| self.images.get(&p)).and_then(|ci| ci.cell) {
            self.env.graphics.set_cell(cell.0, cell.1);
        }
    }

    /// Programs may send images while an attached client can show them.
    fn update_availability(&mut self) {
        let any = self.images.iter().any(|(id, ci)| ci.capable && self.clients.get(id).is_some_and(|c| c.attached));
        self.env.graphics.set_available(any);
    }

    /// Whether the cells of images render as images for this client.
    pub(super) fn can_show_images(&self, id: ClientId) -> bool {
        self.images.get(&id).is_some_and(|ci| ci.capable)
    }

    /// What a client's terminal needs before the next frame: images it does not have, new
    /// placements, and the deletion of images no pane shows any more. Within a budget; true
    /// when more is left for the next frames.
    pub(super) fn graphics_updates(&mut self, id: ClientId) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let version = self.env.graphics.version();
        let Some(ci) = self.images.get(&id) else { return (out, false) };
        if !ci.capable || ci.version == Some(version) {
            return (out, false);
        }
        type Live = (u64, Arc<Picture>, Vec<(u32, u16, u16)>);
        let live: HashMap<u32, Live> = self
            .panes
            .values()
            .filter_map(|p| p.term())
            .flat_map(|t| t.graphics.shown())
            .map(|(kid, s)| (kid, (s.generation, s.picture.clone(), s.placements.clone())))
            .collect();
        let Some(ci) = self.images.get_mut(&id) else { return (out, false) };
        let gone: Vec<u32> = ci.sent.keys().filter(|k| !live.contains_key(k)).copied().collect();
        for kid in gone {
            graphics::delete(kid, &mut out);
            ci.sent.remove(&kid);
        }
        let mut more = false;
        for (kid, (generation, picture, placements)) in live {
            let had = ci.sent.get(&kid).copied();
            if had == Some(generation) {
                continue;
            }
            if had.is_none() {
                // One large image may go alone; then the others wait.
                if !out.is_empty() && out.len() + picture.png.len() * 4 / 3 > BUDGET {
                    more = true;
                    continue;
                }
                graphics::transmit(kid, &picture, &mut out);
            }
            for (placement, cols, rows) in placements {
                graphics::place(kid, placement, cols, rows, &mut out);
            }
            ci.sent.insert(kid, generation);
        }
        if !more {
            ci.version = Some(version);
        }
        (out, more)
    }
}
