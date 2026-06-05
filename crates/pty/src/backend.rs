//! Boundary value types, the typed error, and the [`PtyBackend`] factory trait.

use std::path::PathBuf;

use crate::session::PtySession;

/// Terminal grid size in character cells. Pixel dimensions are deliberately
/// omitted — nothing in the stack needs them until a GPU renderer exists
/// (PoC 2); `Default` is derived so adding them later is non-breaking for
/// struct-literal call sites using `..Default::default()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

/// What to run inside the PTY.
///
/// `cwd`/`env` are part of the boundary from day one: the mux groups sessions
/// by working directory and tags them with metadata (roadmap §6), so leaving
/// them out would force a breaking trait change right when L3 starts. Session
/// metadata itself (tags, agent type, group keys) is strictly an L3 concern
/// and must NOT live here.
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

/// Typed failure modes of the L0 boundary.
///
/// Consumers (the future mux supervisor) branch on these: [`Self::AlreadyReaped`]
/// is a benign race outcome (stop escalating), while [`Self::Kill`] is a real
/// delivery failure (escalate). A stringly-typed error cannot carry that
/// distinction.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("failed to open PTY")]
    Open(#[source] std::io::Error),
    #[error("spawn of '{program}' in PTY failed")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("PTY resize failed")]
    Resize(#[source] std::io::Error),
    #[error("failed to clone PTY reader")]
    Reader(#[source] std::io::Error),
    #[error("failed to acquire the PTY writer")]
    WriterInit(#[source] std::io::Error),
    #[error("PTY writer already taken")]
    WriterTaken,
    #[error("session is closed")]
    Closed,
    #[error("child already reaped — refusing to signal a possibly reused PID")]
    AlreadyReaped,
    #[error("kill not delivered")]
    Kill(#[source] std::io::Error),
    #[error("failed to wait for PTY child")]
    Wait(#[source] std::io::Error),
}

/// Factory for PTY sessions.
pub trait PtyBackend {
    /// Opens a PTY of the given size and spawns `cmd` inside it.
    ///
    /// L3 seam: process-tree containment (Windows Job Object /
    /// Unix `setsid` + process group) must be set up HERE, at spawn time —
    /// it cannot be retrofitted onto a running child. See roadmap Приложение A.
    fn spawn(&self, cmd: &SpawnCommand, size: PtySize) -> Result<Box<dyn PtySession>, PtyError>;
}
