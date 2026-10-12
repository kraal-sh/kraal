//! A pane's archived lines, shared with the thread that encodes the rows output pushes out of
//! the emulator: the parser hands the rows over as they are and goes on (see `archive.rs`). One
//! thread serves every pane; whoever reads a pane's lines stores what that thread has not taken
//! of them yet, and so never waits for another pane's rows.

use std::collections::VecDeque;
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
    /// Batches handed over and not taken by the worker yet, oldest first.
    queue: VecDeque<Batch>,
    /// The worker is storing a batch it took.
    storing: bool,
    /// Rows stored, to take the place of the next ones handed over in the emulator's grid; none
    /// are kept while the pane rests.
    spare: Vec<Row<Cell>>,
    resting: bool,
    /// A batch could not be encoded (a bug): its lines are lost.
    failed: bool,
}

/// Rows moved out of the emulator, oldest first, and the lines they make.
pub(super) struct Batch {
    pub(super) rows: Vec<Row<Cell>>,
    pub(super) lines: Vec<Piece>,
    /// Then the oldest lines go while they take more rows than this.
    pub(super) trim: usize,
}

impl Batch {
    /// No rows: the oldest lines go while they take more rows than `trim`.
    pub(super) fn trim(trim: usize) -> Batch {
        Batch { rows: Vec::new(), lines: Vec::new(), trim }
    }
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
    /// The lines, once the batches handed over are stored: the one the worker is storing, and
    /// those it has not taken yet, stored here.
    pub(super) fn lines(&self) -> Lines<'_> {
        let mut shelf = lock(&self.shelf);
        while shelf.storing {
            shelf = self.stored.wait(shelf).unwrap_or_else(PoisonError::into_inner);
        }
        let mut encoder = Encoder::default();
        while let Some(batch) = shelf.queue.pop_front() {
            let encoded = encode(&batch, &mut encoder);
            drop(shelf.store(batch, encoded));
        }
        debug_assert!(!shelf.failed, "a batch of rows could not be stored");
        Lines(shelf)
    }

    /// Hand `batch` over to be stored, after those before: here, without a worker.
    pub(super) fn send(self: &Arc<Self>, batch: Batch) {
        lock(&self.shelf).queue.push_back(batch);
        if worker().is_none_or(|w| w.send(Work::Store(self.clone())).is_err()) {
            drop(self.lines());
        }
    }

    /// Once the rows handed over are stored and let go of, the memory freed goes back to the
    /// system (see `release_memory`).
    pub(super) fn release(&self) {
        if let Some(worker) = worker() {
            let _ = worker.send(Work::Release);
        }
    }

    /// Batches handed over and not stored yet.
    pub(super) fn backlog(&self) -> usize {
        let shelf = lock(&self.shelf);
        shelf.queue.len() + usize::from(shelf.storing)
    }

    /// Add to `out` up to `n` rows `columns` wide that were stored.
    pub(super) fn take_spare(&self, columns: usize, n: usize, out: &mut Vec<Row<Cell>>) {
        let mut shelf = lock(&self.shelf);
        shelf.resting = false;
        shelf.spare.retain(|r| r.len() == columns);
        let at = shelf.spare.len().saturating_sub(n);
        out.extend(shelf.spare.drain(at..));
    }

    /// Let go of the rows kept for the next batches, and keep none until the next.
    pub(super) fn free_spare(&self) {
        let spare = {
            let mut shelf = lock(&self.shelf);
            shelf.resting = true;
            std::mem::take(&mut shelf.spare)
        };
        drop(spare);
    }

    /// Store the oldest batch not taken yet, if any: the worker's part. It is encoded without
    /// the lock: reading the lines waits only for the copy.
    fn store_next(&self, encoder: &mut Encoder) {
        let batch = {
            let mut shelf = lock(&self.shelf);
            let Some(batch) = shelf.queue.pop_front() else { return };
            shelf.storing = true;
            batch
        };
        let encoded = encode(&batch, encoder);
        let freed = {
            let mut shelf = lock(&self.shelf);
            shelf.storing = false;
            shelf.store(batch, encoded)
        };
        self.stored.notify_all();
        drop(freed);
    }
}

/// What the worker is asked to do.
enum Work {
    /// Store the store's oldest batch, unless it was stored meanwhile.
    Store(Arc<Store>),
    Release,
}

/// The thread that stores the batches, started with the first.
fn worker() -> Option<&'static Sender<Work>> {
    static WORKER: OnceLock<Option<Sender<Work>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (tx, rx) = crossbeam_channel::unbounded::<Work>();
            let thread = std::thread::Builder::new().name("scrollback".into()).spawn(move || {
                let mut encoder = Encoder::default();
                let mut release = false;
                while let Ok(work) = rx.recv() {
                    match work {
                        Work::Store(store) => store.store_next(&mut encoder),
                        Work::Release => release = true,
                    }
                    // Only when asked (a pane rests, a long view went): while output goes on, the
                    // memory would be asked for again at once.
                    if release && rx.is_empty() {
                        super::release_memory();
                        release = false;
                    }
                }
            });
            thread.ok().map(|_| tx)
        })
        .as_ref()
}

/// A batch's lines, encoded one after the other: their records, and where each one's ends, with
/// the rows the line took and the images it shows.
struct Encoded {
    records: Vec<u8>,
    ends: Vec<(usize, usize, Vec<u32>)>,
}

/// The lines of `batch`, encoded; None when that failed (a bug).
fn encode(batch: &Batch, encoder: &mut Encoder) -> Option<Encoded> {
    let mut records = Vec::new();
    let mut ends = Vec::with_capacity(batch.lines.len());
    let encoded = catch_unwind(AssertUnwindSafe(|| {
        let mut cells: Vec<&[Cell]> = Vec::new();
        let mut at = 0;
        for piece in &batch.lines {
            let n = piece.rows as usize;
            cells.clear();
            cells.extend(batch.rows[at..at + n].iter().map(|r| &r[..]));
            let images = encoder.encode(&cells, piece.wraps, usize::MAX, &mut records);
            #[cfg(test)]
            {
                // Laid out again at the same width, the line takes the rows it took.
                let start = ends.last().map_or(0, |(end, _, _)| *end);
                let line = super::history::Line::parse(&records[start..]);
                assert_eq!(line.rows_at(cells[0].len()), n);
            }
            ends.push((records.len(), n, images));
            at += n;
        }
    }));
    if encoded.is_err() {
        *encoder = Encoder::default();
        return None;
    }
    Some(Encoded { records, ends })
}

impl Shelf {
    /// Add the lines of `batch` as `encoded`, and trim. Returns the rows to let go of (better
    /// without the lock): the others are kept for the next batches.
    fn store(&mut self, batch: Batch, encoded: Option<Encoded>) -> Vec<Row<Cell>> {
        let Batch { mut rows, trim, .. } = batch;
        match encoded {
            Some(Encoded { records, ends }) => {
                let width = rows.first().map_or(0, |r| r.len());
                let mut start = 0;
                for (end, n, images) in &ends {
                    self.lines.append(&records[start..*end], *n, width, images);
                    start = *end;
                }
                self.lines.trim(trim, false);
            }
            None => self.failed = true,
        }
        let kept = if self.resting { 0 } else { SPARE.saturating_sub(self.spare.len()) };
        let freed = rows.split_off(kept.min(rows.len()));
        self.spare.append(&mut rows);
        freed
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::index::Column;

    use super::*;

    /// `n` lines of a row each, of text, `width` wide.
    fn batch(n: usize, width: usize) -> Batch {
        let mut row: Row<Cell> = Row::new(width);
        for (i, c) in "a line of text".chars().enumerate() {
            row[Column(i)].c = c;
        }
        Batch { rows: vec![row; n], lines: vec![Piece { rows: 1, wraps: false }; n], trim: usize::MAX }
    }

    #[test]
    fn reading_a_panes_lines_waits_for_no_other_pane() {
        let (busy, idle) = (Arc::new(Store::default()), Arc::new(Store::default()));
        busy.send(batch(20_000, 80));
        idle.send(batch(10, 80));
        assert_eq!(idle.lines().len(), 10);
        assert!(busy.backlog() > 0, "the other pane's rows were stored first");
        assert_eq!(busy.lines().len(), 20_000);
        assert_eq!(busy.backlog(), 0);
    }
}
