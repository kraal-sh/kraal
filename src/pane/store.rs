//! A pane's archived lines, shared with the thread that encodes the rows output pushes out of
//! the emulator: the parser hands the rows over as they are and goes on (see `archive.rs`).

use std::ops::{Deref, DerefMut};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};

use alacritty_terminal::grid::Row;
use alacritty_terminal::term::cell::Cell;
use crossbeam_channel::Sender;

use super::history::{Encoder, History};

/// Rows a pane keeps for the next batch, at most.
const SPARE: usize = 1024;

#[derive(Default)]
pub(super) struct Store {
    shelf: Mutex<Shelf>,
    stored: Condvar,
}

#[derive(Default)]
struct Shelf {
    lines: History,
    /// Batches handed over and not stored yet.
    pending: usize,
    /// Rows stored, to take the place of the next ones handed over in the emulator's grid.
    spare: Vec<Row<Cell>>,
    /// A batch could not be encoded (a bug): its lines are lost.
    failed: bool,
}

/// Rows moved out of the emulator, oldest first, and the lines they make.
#[derive(Default)]
pub(super) struct Batch {
    pub(super) rows: Vec<Row<Cell>>,
    pub(super) lines: Vec<Piece>,
    /// Then the oldest lines go while they take more rows than this.
    pub(super) trim: usize,
}

/// A line of a batch: its rows, and whether its last one wraps onto the next line (it was cut).
#[derive(Clone, Copy)]
pub(super) struct Piece {
    pub(super) rows: u32,
    pub(super) wraps: bool,
}

/// The lines, with every batch handed over stored.
pub(super) struct Lines<'a>(MutexGuard<'a, Shelf>);

impl Deref for Lines<'_> {
    type Target = History;

    fn deref(&self) -> &History {
        &self.0.lines
    }
}

impl DerefMut for Lines<'_> {
    fn deref_mut(&mut self) -> &mut History {
        &mut self.0.lines
    }
}

fn lock(shelf: &Mutex<Shelf>) -> MutexGuard<'_, Shelf> {
    shelf.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Store {
    /// The lines, once the batches handed over are stored.
    pub(super) fn lines(&self) -> Lines<'_> {
        let mut shelf = lock(&self.shelf);
        while shelf.pending > 0 {
            shelf = self.stored.wait(shelf).unwrap_or_else(PoisonError::into_inner);
        }
        debug_assert!(!shelf.failed, "a batch of rows could not be stored");
        Lines(shelf)
    }

    /// Hand `batch` over to be stored, after those before.
    pub(super) fn send(self: &Arc<Self>, batch: Batch) {
        lock(&self.shelf).pending += 1;
        let job = Job { store: self.clone(), batch };
        match worker() {
            Some(worker) => {
                if let Err(e) = worker.send(job) {
                    e.into_inner().run(&mut Encoder::default());
                }
            }
            None => job.run(&mut Encoder::default()),
        }
    }

    /// Batches handed over and not stored yet.
    pub(super) fn backlog(&self) -> usize {
        lock(&self.shelf).pending
    }

    /// Add to `out` up to `n` rows `columns` wide that were stored.
    pub(super) fn take_spare(&self, columns: usize, n: usize, out: &mut Vec<Row<Cell>>) {
        let mut shelf = lock(&self.shelf);
        shelf.spare.retain(|r| r.len() == columns);
        let at = shelf.spare.len().saturating_sub(n);
        out.extend(shelf.spare.drain(at..));
    }

    /// Let go of the rows kept for the next batches.
    pub(super) fn free_spare(&self) {
        lock(&self.shelf).spare = Vec::new();
    }
}

struct Job {
    store: Arc<Store>,
    batch: Batch,
}

/// The thread that stores the batches, started with the first.
fn worker() -> Option<&'static Sender<Job>> {
    static WORKER: OnceLock<Option<Sender<Job>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (tx, rx) = crossbeam_channel::unbounded::<Job>();
            let thread = std::thread::Builder::new().name("scrollback".into()).spawn(move || {
                let mut encoder = Encoder::default();
                for job in rx {
                    job.run(&mut encoder);
                }
            });
            thread.ok().map(|_| tx)
        })
        .as_ref()
}

impl Job {
    fn run(self, encoder: &mut Encoder) {
        let Job { store, batch: Batch { mut rows, lines, trim } } = self;
        let width = rows.first().map_or(0, |r| r.len());
        // Encoded before taking the lock: reading the lines waits only for the copy.
        let mut records = Vec::new();
        let mut ends = Vec::with_capacity(lines.len());
        let encoded = catch_unwind(AssertUnwindSafe(|| {
            let mut cells: Vec<&[Cell]> = Vec::new();
            let mut at = 0;
            for piece in &lines {
                let n = piece.rows as usize;
                cells.clear();
                cells.extend(rows[at..at + n].iter().map(|r| &r[..]));
                let images = encoder.encode(&cells, piece.wraps, usize::MAX, &mut records);
                #[cfg(test)]
                {
                    // Laid out again at the same width, the line takes the rows it took.
                    let start = ends.last().map_or(0, |(end, _, _)| *end);
                    let line = super::history::Line::parse(&records[start..]);
                    assert_eq!(line.rows_at(width), n);
                }
                ends.push((records.len(), n, images));
                at += n;
            }
        }));
        let mut shelf = lock(&store.shelf);
        if encoded.is_ok() {
            let mut start = 0;
            for (end, n, images) in &ends {
                shelf.lines.append(&records[start..*end], *n, width, images);
                start = *end;
            }
            shelf.lines.trim(trim, false);
        } else {
            shelf.failed = true;
            *encoder = Encoder::default();
        }
        shelf.pending -= 1;
        rows.truncate(SPARE.saturating_sub(shelf.spare.len()));
        shelf.spare.append(&mut rows);
        drop(shelf);
        store.stored.notify_all();
    }
}
