//! `FakePtyBackend` — an in-memory, scripted implementation of the L0 traits.
//!
//! Purpose: (a) deterministic tests for upper layers (PoC 1's terminal model
//! parses scripted byte chunks, no real shell involved); (b) the living proof
//! that the boundary is actually swappable (roadmap §10) — the contract tests
//! run the same assertions against this and the real backend.
//!
//! The fake mirrors the real backend's *contracts*, including the subtle one:
//! readers reach EOF only once the session is closed (or killed), exactly
//! like ConPTY. A fake that EOFs eagerly would green-light consumers that
//! deadlock against the real thing.

use std::collections::VecDeque;
use std::io::{Cursor, Read, Write};
use std::sync::{Arc, Condvar, Mutex};

use crate::backend::{PtyBackend, PtyError, PtyExitStatus, PtySize, SpawnCommand};
use crate::session::{PtyKiller, PtyResizer, PtySession, ReapFlag};

/// When the scripted child "exits".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeExit {
    /// The child wrote its whole script and exited immediately (like `echo`):
    /// `wait()` returns at once; readers still drain the buffered tail and —
    /// mirroring ConPTY — reach EOF only after `close()`.
    Immediate,
    /// The child "runs" until killed (or the session is closed); `wait()`
    /// blocks until then.
    OnKill,
}

/// Script for one fake session.
#[derive(Debug, Clone)]
pub struct FakeScript {
    /// Output delivered to readers chunk-by-chunk, exactly as scripted —
    /// chunk boundaries are preserved so incremental parsing is exercised.
    pub output_chunks: Vec<Vec<u8>>,
    /// Status reported by `wait()`/`try_wait()`.
    pub exit: PtyExitStatus,
    pub exit_mode: FakeExit,
}

impl Default for FakeScript {
    fn default() -> Self {
        Self {
            output_chunks: Vec::new(),
            exit: PtyExitStatus {
                code: 0,
                success: true,
            },
            exit_mode: FakeExit::Immediate,
        }
    }
}

/// Everything the fake records, shared with the [`FakeProbe`].
#[derive(Default)]
struct FakeShared {
    state: Mutex<FakeState>,
    cond: Condvar,
    reaped: ReapFlag,
}

#[derive(Default)]
struct FakeState {
    spawned: Option<(SpawnCommand, PtySize)>,
    pending_output: VecDeque<Vec<u8>>,
    written: Vec<u8>,
    resizes: Vec<PtySize>,
    killed: bool,
    closed: bool,
}

impl FakeShared {
    /// Whether the scripted child has exited (independent of readers: output
    /// is "already in the pty buffer", like a real fast child's).
    fn exited(&self, st: &FakeState, mode: FakeExit) -> bool {
        match mode {
            FakeExit::Immediate => true,
            FakeExit::OnKill => st.killed || st.closed,
        }
    }
}

/// Test-side window into a fake session: what was spawned, written, resized.
pub struct FakeProbe {
    shared: Arc<FakeShared>,
}

impl FakeProbe {
    pub fn spawned(&self) -> Option<(SpawnCommand, PtySize)> {
        self.shared.state.lock().unwrap().spawned.clone()
    }
    /// Everything the consumer wrote into the child's stdin so far.
    pub fn written(&self) -> Vec<u8> {
        self.shared.state.lock().unwrap().written.clone()
    }
    pub fn resizes(&self) -> Vec<PtySize> {
        self.shared.state.lock().unwrap().resizes.clone()
    }
    pub fn was_killed(&self) -> bool {
        self.shared.state.lock().unwrap().killed
    }
}

/// Scripted [`PtyBackend`]. One backend instance scripts ONE session; a
/// second `spawn` is refused (tests that need several sessions create
/// several backends).
pub struct FakePtyBackend {
    script: FakeScript,
    shared: Arc<FakeShared>,
}

impl FakePtyBackend {
    pub fn new(script: FakeScript) -> (Self, FakeProbe) {
        let shared = Arc::new(FakeShared::default());
        let probe = FakeProbe {
            shared: Arc::clone(&shared),
        };
        (Self { script, shared }, probe)
    }
}

impl PtyBackend for FakePtyBackend {
    fn spawn(&self, cmd: &SpawnCommand, size: PtySize) -> Result<Box<dyn PtySession>, PtyError> {
        let mut st = self.shared.state.lock().unwrap();
        if st.spawned.is_some() {
            // Harness misuse, reported as a spawn failure (the variant a
            // caller already handles), never as a PTY-open failure.
            return Err(PtyError::Spawn {
                program: cmd.program.clone(),
                source: std::io::Error::other(
                    "FakePtyBackend scripts a single session; spawn called twice",
                ),
            });
        }
        st.spawned = Some((cmd.clone(), size));
        st.pending_output = self.script.output_chunks.iter().cloned().collect();
        drop(st);

        Ok(Box::new(FakeSession {
            shared: Arc::clone(&self.shared),
            script: self.script.clone(),
            writer_taken: false,
        }))
    }
}

struct FakeSession {
    shared: Arc<FakeShared>,
    script: FakeScript,
    writer_taken: bool,
}

impl FakeSession {
    fn reap(&mut self) -> PtyExitStatus {
        self.shared.reaped.mark();
        self.script.exit
    }
}

/// Single resize recorder shared by the session and the detached resizer, so
/// the two entry points cannot drift (mirrors portable.rs's resize_master).
fn record_resize(shared: &FakeShared, size: PtySize) -> Result<(), PtyError> {
    let mut st = shared.state.lock().unwrap();
    if st.closed {
        return Err(PtyError::Closed);
    }
    st.resizes.push(size);
    Ok(())
}

impl PtySession for FakeSession {
    fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        record_resize(&self.shared, size)
    }

    fn reader(&self) -> Result<Box<dyn Read + Send>, PtyError> {
        let st = self.shared.state.lock().unwrap();
        if st.closed {
            return Err(PtyError::Closed);
        }
        drop(st);
        Ok(Box::new(FakeReader {
            shared: Arc::clone(&self.shared),
            carry: Cursor::new(Vec::new()),
        }))
    }

    fn take_writer(&mut self) -> Result<Box<dyn Write + Send>, PtyError> {
        if self.writer_taken {
            return Err(PtyError::WriterTaken);
        }
        self.writer_taken = true;
        Ok(Box::new(FakeWriter {
            shared: Arc::clone(&self.shared),
        }))
    }

    fn killer(&self) -> Box<dyn PtyKiller> {
        Box::new(FakeKiller {
            shared: Arc::clone(&self.shared),
        })
    }

    fn resizer(&self) -> Box<dyn PtyResizer> {
        Box::new(FakeResizer {
            shared: std::sync::Arc::downgrade(&self.shared),
        })
    }

    fn wait(&mut self) -> Result<PtyExitStatus, PtyError> {
        let mut st = self.shared.state.lock().unwrap();
        while !self.shared.exited(&st, self.script.exit_mode) {
            st = self.shared.cond.wait(st).unwrap();
        }
        drop(st);
        Ok(self.reap())
    }

    fn try_wait(&mut self) -> Result<Option<PtyExitStatus>, PtyError> {
        let st = self.shared.state.lock().unwrap();
        let exited = self.shared.exited(&st, self.script.exit_mode);
        drop(st);
        Ok(exited.then(|| self.reap()))
    }

    fn close(&mut self) -> Result<(), PtyError> {
        let mut st = self.shared.state.lock().unwrap();
        st.closed = true;
        self.shared.cond.notify_all();
        Ok(())
    }
}

impl Drop for FakeSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct FakeReader {
    shared: Arc<FakeShared>,
    /// Remainder of a scripted chunk that didn't fit the caller's buffer —
    /// a battle-tested `io::Cursor` instead of hand-rolled index juggling.
    carry: Cursor<Vec<u8>>,
}

impl FakeReader {
    fn carry_drained(&self) -> bool {
        self.carry.position() >= self.carry.get_ref().len() as u64
    }
}

impl Read for FakeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.carry_drained() {
            return self.carry.read(buf);
        }
        let mut st = self.shared.state.lock().unwrap();
        loop {
            if let Some(chunk) = st.pending_output.pop_front() {
                // Skip empty scripted chunks: returning their Ok(0) would
                // synthesize a false EOF before close()/kill — the eager-EOF
                // behavior this fake exists to never exhibit.
                if chunk.is_empty() {
                    continue;
                }
                drop(st);
                self.carry = Cursor::new(chunk);
                return self.carry.read(buf);
            }
            // Mirror ConPTY exactly: EOF only once the session is closed (or
            // the child killed) — never merely because the child exited. A
            // fake that EOFs eagerly would pass consumers that deadlock
            // against the real backend.
            if st.closed || st.killed {
                return Ok(0);
            }
            st = self.shared.cond.wait(st).unwrap();
        }
    }
}

struct FakeWriter {
    shared: Arc<FakeShared>,
}

impl Write for FakeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut st = self.shared.state.lock().unwrap();
        if st.closed {
            return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        }
        st.written.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FakeKiller {
    shared: Arc<FakeShared>,
}

impl PtyKiller for FakeKiller {
    fn kill(&mut self) -> Result<(), PtyError> {
        self.shared.reaped.check()?;
        let mut st = self.shared.state.lock().unwrap();
        st.killed = true;
        self.shared.cond.notify_all();
        Ok(())
    }
}

struct FakeResizer {
    shared: std::sync::Weak<FakeShared>,
}

impl PtyResizer for FakeResizer {
    fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        let Some(shared) = self.shared.upgrade() else {
            return Err(PtyError::Closed);
        };
        record_resize(&shared, size)
    }
}
