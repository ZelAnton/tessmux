//! The live-session traits: [`PtySession`] and its detached handles.
//!
//! # Session lifecycle model
//!
//! A session conceptually moves through three states (a documentation model
//! for now — the L3 supervisor will own the runtime state machine):
//!
//! ```text
//! Running ──wait()/try_wait()=Some──▶ Exited ──close()──▶ Closed
//!     └────────────close()──────────────────────────────▶ Closed
//! ```
//!
//! | State   | reader            | take_writer | resize        | kill            | wait            |
//! |---------|-------------------|-------------|---------------|-----------------|-----------------|
//! | Running | streams           | once        | applies       | delivers        | blocks          |
//! | Exited  | drains tail       | once        | applies       | `AlreadyReaped` | returns status  |
//! | Closed  | EOF               | once*       | `Closed`      | best-effort     | returns status  |
//!
//! (*the writer endpoint, once taken, outlives `close()` and surfaces broken
//! pipes as ordinary `io::Error`s.)
//!
//! The implementation guards are facets of this one machine: the `Option`
//! writer (single take), the `reaped` flag (kill-after-reap refusal), and the
//! `Weak` resizer (no handle may keep a closed console alive).
//!
//! # L3 seams (recorded, not built)
//!
//! - A future detached *input* handle must hold a `Weak` reference like
//!   [`PtySession::resizer`] does — a strong clone in a GUI widget would keep
//!   the pseudo console open and deadlock teardown.
//! - `PtyKiller`/`PtyResizer` will fold into one cloneable `PtyController`
//!   when the mux needs per-pane handles; tree-kill with escalation
//!   (`kill_tree(grace)`) replaces `kill` then. See roadmap Приложение A.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::{PtyError, PtyExitStatus, PtySize};

/// One live PTY with a child process attached.
///
/// `killer()`/`resizer()` return detached handles usable from other threads
/// while the owner blocks in [`Self::wait`] (which takes `&mut self`).
pub trait PtySession: Send {
    fn resize(&self, size: PtySize) -> Result<(), PtyError>;

    /// Clonable output stream (PTY -> caller). May be called multiple times.
    ///
    /// EOF contract: readers reach EOF once the session is closed (via
    /// [`Self::close`] or drop) — on Windows the ConPTY output pipe stays
    /// open for the pseudo console's lifetime, even after the child exits.
    /// Call `close()` before joining a thread that reads to EOF.
    fn reader(&self) -> Result<Box<dyn Read + Send>, PtyError>;

    /// Input stream (caller -> PTY). Single-use — the value is moved out;
    /// the second call returns [`PtyError::WriterTaken`].
    ///
    /// Dropping the writer signals end-of-input, but how much that means is
    /// platform-dependent: on Windows it closes the child's input pipe (a
    /// true stdin EOF); on Unix it only emits a VEOF byte, which the child
    /// sees as EOF solely in canonical line-discipline mode.
    fn take_writer(&mut self) -> Result<Box<dyn Write + Send>, PtyError>;

    /// Detached kill handle for the child.
    fn killer(&self) -> Box<dyn PtyKiller>;

    /// Detached resize handle for the PTY.
    fn resizer(&self) -> Box<dyn PtyResizer>;

    /// Blocks until the child exits; returns its exit status.
    fn wait(&mut self) -> Result<PtyExitStatus, PtyError>;

    /// Non-blocking probe: `Some(status)` once the child has exited, `None`
    /// while it is still running. Lets a supervisor reap many sessions
    /// without parking one blocking thread per child.
    ///
    /// Windows caveats inherited from the backend (portable-pty ≤ 0.9): a
    /// child that exits with code 259 (`STILL_ACTIVE`) is reported as still
    /// running, and a failing `GetExitCodeProcess` also reads as "running".
    /// A supervisor should pair polling with a liveness backstop ([`Self::wait`]
    /// on a watchdog, or kill escalation at L3).
    fn try_wait(&mut self) -> Result<Option<PtyExitStatus>, PtyError>;

    /// Begins orderly teardown: closes the master / pseudo console.
    ///
    /// POSTCONDITION: after `close()` returns, every reader clone drains and
    /// reaches EOF — it can no longer block indefinitely on the console
    /// staying open, so joining a pump thread after `close()` cannot
    /// deadlock. The EOF is prompt but not instantaneous: on Windows it
    /// lands once conhost finishes tearing down (bounded-async, typically
    /// milliseconds). Idempotent; legal after `wait()`.
    ///
    /// NOTE: closing while the child is still running may terminate it
    /// (on Windows, pseudo-console clients are torn down with the console).
    fn close(&mut self) -> Result<(), PtyError>;
}

/// Detached kill handle for a [`PtySession`]'s child.
pub trait PtyKiller: Send {
    /// Terminates the child. Platform caveats:
    /// - If the child already exited but is not yet reaped, Windows errors
    ///   while Unix succeeds — treat `Err` as "kill not delivered", never as
    ///   proof the child is still alive.
    /// - On Unix this sends a single SIGHUP (no SIGKILL escalation): a child
    ///   that ignores SIGHUP survives.
    /// - Once [`PtySession::wait`]/`try_wait` has reaped the child, `kill`
    ///   refuses to fire with [`PtyError::AlreadyReaped`]: the underlying
    ///   Unix kill targets a raw PID, and a reaped PID can be reused. A
    ///   microsecond check-then-reap race remains, but the practically
    ///   unbounded window of a long-lived detached killer is closed.
    fn kill(&mut self) -> Result<(), PtyError>;
}

/// Detached resize handle for a [`PtySession`]'s PTY. Holds only a weak
/// reference: a long-lived clone cannot keep a closed console alive.
pub trait PtyResizer: Send {
    fn resize(&self, size: PtySize) -> Result<(), PtyError>;
}

/// The one shared implementation of the kill-after-reap contract (see
/// [`PtyKiller::kill`]): `mark()` at every reap site, `check()` in every
/// killer. A single definition keeps the two backends (and the memory
/// orderings pairing the store with the load) from drifting apart.
#[derive(Clone, Default)]
pub(crate) struct ReapFlag(Arc<AtomicBool>);

impl ReapFlag {
    /// Latch "the child has been reaped" — its PID may be reused from here on.
    pub(crate) fn mark(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Refuse the kill once reaped.
    pub(crate) fn check(&self) -> Result<(), PtyError> {
        if self.0.load(Ordering::Acquire) {
            return Err(PtyError::AlreadyReaped);
        }
        Ok(())
    }
}
