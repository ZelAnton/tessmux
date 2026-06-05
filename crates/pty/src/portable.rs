//! The `portable-pty` implementation of the L0 traits — the ONLY module that
//! names `portable_pty`. Swapping the backend (roadmap §2/§10) means
//! replacing this one file behind unchanged traits.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, Weak};

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, native_pty_system};

use crate::backend::{PtyBackend, PtyError, PtyExitStatus, PtySize, SpawnCommand};
use crate::session::{PtyKiller, PtyResizer, PtySession, ReapFlag};

/// [`PtyBackend`] over `portable-pty` (`native_pty_system()` — ConPTY on
/// Windows, openpty on Unix).
pub struct PortablePtyBackend;

/// portable-pty surfaces `anyhow::Error` from several APIs; flatten to
/// `io::Error` for the typed boundary without losing the source chain. When
/// the chain's top IS an `io::Error`, recover it so its `ErrorKind`
/// (BrokenPipe, PermissionDenied, …) stays machine-readable instead of
/// collapsing to `Other`.
fn to_io(err: anyhow::Error) -> std::io::Error {
    match err.downcast::<std::io::Error>() {
        Ok(io) => io,
        Err(err) => std::io::Error::other(err),
    }
}

impl PtyBackend for PortablePtyBackend {
    fn spawn(&self, cmd: &SpawnCommand, size: PtySize) -> Result<Box<dyn PtySession>, PtyError> {
        let pair = native_pty_system()
            .openpty(to_native_size(size))
            .map_err(|e| PtyError::Open(to_io(e)))?;

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
            .map_err(|e| PtyError::Spawn {
                program: cmd.program.clone(),
                source: to_io(e),
            })?;
        // Drop the slave: the parent keeps no handle to the child's end, so
        // the master reader sees EOF when the child exits.
        drop(pair.slave);

        // take_writer() up front: MasterPty allows it only once, and deferring
        // it would force interior mutability behind the trait's take_writer().
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::WriterInit(to_io(e)))?;

        Ok(Box::new(PortablePtySession {
            // Mutex'ed shared ownership so resizer() can hand the master to
            // another thread while wait() holds &mut self here; Option so
            // close() can deterministically drop it (-> reader EOF).
            master: Arc::new(Mutex::new(Some(pair.master))),
            writer: Some(writer),
            child,
            reaped: ReapFlag::default(),
        }))
    }
}

type MasterCell = Mutex<Option<Box<dyn MasterPty + Send>>>;
type SharedMaster = Arc<MasterCell>;

struct PortablePtySession {
    master: SharedMaster,
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
    /// Latched once the child is reaped; killers check it so a late kill
    /// can't signal a reused PID (see `PtyKiller` docs).
    reaped: ReapFlag,
}

fn lock_master(
    master: &MasterCell,
) -> std::sync::MutexGuard<'_, Option<Box<dyn MasterPty + Send>>> {
    // A poisoned lock means a panic mid-resize/clone in the backend. The
    // master handle itself stays usable (each operation is independent), so
    // recover the guard instead of cascading the panic into every later
    // caller — e.g. a backend hiccup in the resize poller must not take the
    // whole session down.
    master
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs `f` against the live master, or reports [`PtyError::Closed`] once
/// `close()` has taken it. The single place that decides "closed" — every
/// master-touching operation must come through here so no future method can
/// forget the closed-state arm of the lifecycle table.
fn with_master<T>(
    master: &MasterCell,
    f: impl FnOnce(&(dyn MasterPty + Send)) -> Result<T, PtyError>,
) -> Result<T, PtyError> {
    let guard = lock_master(master);
    let m = guard.as_deref().ok_or(PtyError::Closed)?;
    f(m)
}

/// Single resize implementation shared by the session and the detached
/// resizer, so the two entry points cannot drift.
fn resize_master(master: &MasterCell, size: PtySize) -> Result<(), PtyError> {
    with_master(master, |m| {
        m.resize(to_native_size(size))
            .map_err(|e| PtyError::Resize(to_io(e)))
    })
}

impl PortablePtySession {
    /// Latches the reap flag and converts the status — `&mut` to make "this
    /// call reaps" visible at the signature (the flag itself is atomic for
    /// the detached killers' sake).
    fn status_from(&mut self, status: portable_pty::ExitStatus) -> PtyExitStatus {
        self.reaped.mark();
        PtyExitStatus {
            code: status.exit_code(),
            success: status.success(),
        }
    }
}

impl PtySession for PortablePtySession {
    fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        resize_master(&self.master, size)
    }

    fn reader(&self) -> Result<Box<dyn Read + Send>, PtyError> {
        with_master(&self.master, |m| {
            m.try_clone_reader().map_err(|e| PtyError::Reader(to_io(e)))
        })
    }

    fn take_writer(&mut self) -> Result<Box<dyn Write + Send>, PtyError> {
        self.writer.take().ok_or(PtyError::WriterTaken)
    }

    fn killer(&self) -> Box<dyn PtyKiller> {
        Box::new(PortablePtyKiller {
            killer: self.child.clone_killer(),
            reaped: self.reaped.clone(),
        })
    }

    fn resizer(&self) -> Box<dyn PtyResizer> {
        // Weak, not Arc: a strong clone in a long-lived thread would keep the
        // pseudo console open after close(), and ConPTY readers only see EOF
        // once the console closes — teardown would deadlock.
        Box::new(PortablePtyResizer(Arc::downgrade(&self.master)))
    }

    fn wait(&mut self) -> Result<PtyExitStatus, PtyError> {
        let status = self.child.wait().map_err(PtyError::Wait)?;
        Ok(self.status_from(status))
    }

    fn try_wait(&mut self) -> Result<Option<PtyExitStatus>, PtyError> {
        let status = self.child.try_wait().map_err(PtyError::Wait)?;
        Ok(status.map(|s| self.status_from(s)))
    }

    fn close(&mut self) -> Result<(), PtyError> {
        // Dropping the master closes the pseudo console (ConPTY) / master fd
        // (Unix): every reader clone now drains and reaches EOF — the
        // postcondition the trait promises. Idempotent: a second take() is a
        // no-op. Take under the lock but DROP after releasing it:
        // ClosePseudoConsole can block during conhost teardown, and holding
        // the mutex through it would stall the detached resizer.
        let master = lock_master(&self.master).take();
        drop(master);
        Ok(())
    }
}

struct PortablePtyKiller {
    killer: Box<dyn ChildKiller + Send + Sync>,
    reaped: ReapFlag,
}

impl PtyKiller for PortablePtyKiller {
    fn kill(&mut self) -> Result<(), PtyError> {
        self.reaped.check()?;
        let result = self.killer.kill();
        // Upstream bug (portable-pty <= 0.9, win/mod.rs WinChildKiller):
        // TerminateProcess returns nonzero BOOL on SUCCESS, but the code does
        // `if res != 0 { Err(last_os_error) } else { Ok(()) }` — success
        // comes back as Err carrying a stale error code, failure as Ok. Flip
        // it back here. The real failure code is discarded upstream, so
        // failures surface without one. portable-pty is pinned to 0.8.x in
        // the workspace manifest — RE-VERIFY this shim on any upgrade; it
        // dies for good with the L3 kill_tree redesign (Job Objects).
        #[cfg(windows)]
        let result = match result {
            Err(_) => Ok(()),
            Ok(()) => Err(std::io::Error::other(
                "TerminateProcess failed (error code discarded by portable-pty)",
            )),
        };
        result.map_err(PtyError::Kill)
    }
}

struct PortablePtyResizer(Weak<MasterCell>);

impl PtyResizer for PortablePtyResizer {
    fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        let Some(master) = self.0.upgrade() else {
            return Err(PtyError::Closed);
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
