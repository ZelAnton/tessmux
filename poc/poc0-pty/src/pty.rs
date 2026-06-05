//! L0 boundary: PTY backend traits and the `portable-pty` implementation.
//!
//! Callers (here: `main.rs`, later: the mux layer) program against
//! [`PtyBackend`]/[`PtySession`] only — no `portable-pty` types leak out, so
//! the implementation stays swappable (roadmap §2 / §10). This module moves to
//! `crates/pty` once PoC 1 needs it from a second crate.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result, bail};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, native_pty_system};

/// Terminal grid size in character cells. Pixel dimensions are deliberately
/// omitted — nothing in the stack needs them until a GPU renderer exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

/// What to run inside the PTY.
///
/// `cwd`/`env` are part of the boundary from day one: the mux groups sessions
/// by working directory and tags them with metadata (roadmap §6), so leaving
/// them out would force a breaking trait change right when L3 starts.
#[derive(Debug, Clone, Default)]
pub struct SpawnCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Initial working directory; `None` inherits the parent's.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables overlaid on the inherited environment.
    pub env: Vec<(String, String)>,
}

/// Final status of an exited PTY child.
#[derive(Debug, Clone, Copy)]
pub struct PtyExitStatus {
    pub code: u32,
    /// Whether the platform considers the exit clean. `code == 0` is NOT a
    /// portable equivalent — signal-killed Unix children encode differently.
    pub success: bool,
}

/// Factory for PTY sessions.
pub trait PtyBackend {
    /// Opens a PTY of the given size and spawns `cmd` inside it.
    fn spawn(&self, cmd: &SpawnCommand, size: PtySize) -> Result<Box<dyn PtySession>>;
}

/// One live PTY with a child process attached.
///
/// `killer()`/`resizer()` return detached handles usable from other threads
/// while the owner blocks in [`Self::wait`] (which takes `&mut self`).
pub trait PtySession: Send {
    fn resize(&self, size: PtySize) -> Result<()>;
    /// Clonable output stream (PTY -> caller). May be called multiple times.
    ///
    /// EOF contract: readers reach EOF only once the session is dropped — on
    /// Windows the ConPTY output pipe stays open for the pseudo console's
    /// lifetime, even after the child exits. Drop the session *before*
    /// joining a thread that reads to EOF, or the join deadlocks.
    fn reader(&self) -> Result<Box<dyn Read + Send>>;
    /// Input stream (caller -> PTY). Single-use: the second call errors.
    ///
    /// Dropping the writer signals end-of-input, but how much that means is
    /// platform-dependent: on Windows it closes the child's input pipe (a
    /// true stdin EOF); on Unix it only emits a VEOF byte, which the child
    /// sees as EOF solely in canonical line-discipline mode.
    fn writer(&mut self) -> Result<Box<dyn Write + Send>>;
    /// Detached kill handle for the child.
    fn killer(&self) -> Box<dyn PtyKiller>;
    /// Detached resize handle for the PTY.
    fn resizer(&self) -> Box<dyn PtyResizer>;
    /// Blocks until the child exits; returns its exit status.
    fn wait(&mut self) -> Result<PtyExitStatus>;
}

/// Detached kill handle for a [`PtySession`]'s child.
pub trait PtyKiller: Send {
    /// Terminates the child. Platform caveats:
    /// - If the child already exited, Windows errors while Unix succeeds —
    ///   treat `Err` as "kill not delivered", never as proof the child is
    ///   still alive.
    /// - On Unix this sends a single SIGHUP (no SIGKILL escalation): a child
    ///   that ignores SIGHUP survives.
    /// - Once [`PtySession::wait`] has reaped the child, `kill` refuses to
    ///   fire (errors): on Unix the underlying kill targets a raw PID, and a
    ///   reaped PID can be reused by an unrelated process. A microsecond
    ///   check-then-reap race remains, but the practically unbounded window
    ///   of a long-lived detached killer is closed.
    fn kill(&mut self) -> Result<()>;
}

/// Detached resize handle for a [`PtySession`]'s PTY.
pub trait PtyResizer: Send {
    fn resize(&self, size: PtySize) -> Result<()>;
}

/// [`PtyBackend`] over `portable-pty` (`native_pty_system()` — ConPTY on
/// Windows, openpty on Unix).
pub struct PortablePtyBackend;

impl PtyBackend for PortablePtyBackend {
    fn spawn(&self, cmd: &SpawnCommand, size: PtySize) -> Result<Box<dyn PtySession>> {
        let pair = native_pty_system()
            .openpty(to_native_size(size))
            .context("failed to open PTY")?;

        let mut builder = CommandBuilder::new(&cmd.program);
        builder.args(&cmd.args);
        if let Some(cwd) = &cmd.cwd {
            builder.cwd(cwd);
        }
        for (key, value) in &cmd.env {
            builder.env(key, value);
        }
        let child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("failed to spawn '{}' in PTY", cmd.program))?;
        // Drop the slave: the parent keeps no handle to the child's end, so the
        // master reader sees EOF when the child exits.
        drop(pair.slave);

        // take_writer() up front: MasterPty allows it only once, and deferring
        // it would force interior mutability behind the trait's &mut writer().
        let writer = pair
            .master
            .take_writer()
            .context("failed to take PTY writer")?;

        Ok(Box::new(PortablePtySession {
            // Mutex'ed shared ownership so resizer() can hand the master to
            // another thread while wait() holds &mut self here.
            master: Arc::new(Mutex::new(pair.master)),
            writer: Some(writer),
            child,
            reaped: Arc::new(AtomicBool::new(false)),
        }))
    }
}

type SharedMaster = Arc<Mutex<Box<dyn MasterPty + Send>>>;

struct PortablePtySession {
    master: SharedMaster,
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
    /// Set by `wait()` once the child is reaped; killers check it so a late
    /// kill can't signal a reused PID (see `PtyKiller` docs).
    reaped: Arc<AtomicBool>,
}

fn lock_master(
    master: &Mutex<Box<dyn MasterPty + Send>>,
) -> std::sync::MutexGuard<'_, Box<dyn MasterPty + Send>> {
    // A poisoned lock means a panic mid-resize/clone in the backend. The
    // master handle itself stays usable (each operation is independent), so
    // recover the guard instead of cascading the panic into every later
    // caller — e.g. a backend hiccup in the resize poller must not take the
    // whole session down.
    master
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Single resize implementation shared by the session and the detached
/// resizer, so the two entry points cannot drift.
fn resize_master(master: &Mutex<Box<dyn MasterPty + Send>>, size: PtySize) -> Result<()> {
    lock_master(master)
        .resize(to_native_size(size))
        .context("failed to resize PTY")
}

impl PtySession for PortablePtySession {
    fn resize(&self, size: PtySize) -> Result<()> {
        resize_master(&self.master, size)
    }

    fn reader(&self) -> Result<Box<dyn Read + Send>> {
        lock_master(&self.master)
            .try_clone_reader()
            .context("failed to clone PTY reader")
    }

    fn writer(&mut self) -> Result<Box<dyn Write + Send>> {
        match self.writer.take() {
            Some(w) => Ok(w),
            None => bail!("PTY writer already taken"),
        }
    }

    fn killer(&self) -> Box<dyn PtyKiller> {
        Box::new(PortablePtyKiller {
            killer: self.child.clone_killer(),
            reaped: Arc::clone(&self.reaped),
        })
    }

    fn resizer(&self) -> Box<dyn PtyResizer> {
        // Weak, not Arc: a strong clone in a long-lived thread would keep the
        // pseudo console open after the session drops, and ConPTY readers
        // only see EOF once the console closes — teardown would deadlock.
        Box::new(PortablePtyResizer(Arc::downgrade(&self.master)))
    }

    fn wait(&mut self) -> Result<PtyExitStatus> {
        let status = self.child.wait().context("failed to wait for PTY child")?;
        // After waitpid the PID is free for reuse (Unix) — block late killers.
        self.reaped.store(true, Ordering::Release);
        Ok(PtyExitStatus {
            code: status.exit_code(),
            success: status.success(),
        })
    }
}

struct PortablePtyKiller {
    killer: Box<dyn ChildKiller + Send + Sync>,
    reaped: Arc<AtomicBool>,
}

impl PtyKiller for PortablePtyKiller {
    fn kill(&mut self) -> Result<()> {
        if self.reaped.load(Ordering::Acquire) {
            bail!("child already reaped — refusing to signal a possibly reused PID");
        }
        self.killer.kill().context("failed to kill PTY child")
    }
}

struct PortablePtyResizer(Weak<Mutex<Box<dyn MasterPty + Send>>>);

impl PtyResizer for PortablePtyResizer {
    fn resize(&self, size: PtySize) -> Result<()> {
        let Some(master) = self.0.upgrade() else {
            bail!("PTY session is closed");
        };
        resize_master(&master, size)
    }
}

fn to_native_size(size: PtySize) -> portable_pty::PtySize {
    // ConPTY's COORD fields are i16: clamp so an oversized u16 can't wrap
    // negative on Windows (silent garbage size or a swallowed failed resize).
    portable_pty::PtySize {
        rows: size.rows.min(i16::MAX as u16),
        cols: size.cols.min(i16::MAX as u16),
        pixel_width: 0,
        pixel_height: 0,
    }
}
