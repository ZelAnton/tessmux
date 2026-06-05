//! tessmux L0: the PTY boundary.
//!
//! Upper layers (the milestone binaries today, `term-model`/`mux` later)
//! program against the traits here — [`PtyBackend`], [`PtySession`] and its
//! detached handles — never against a concrete PTY library, so the
//! implementation stays swappable (roadmap §2/§10). [`PortablePtyBackend`]
//! (`portable-pty`: ConPTY on Windows, openpty on Unix) is the production
//! implementation; `FakePtyBackend` (feature `testing`) is the scripted
//! in-memory one that proves it.

mod backend;
mod portable;
mod pump;
mod session;

#[cfg(any(test, feature = "testing"))]
mod fake;

pub use backend::{PtyBackend, PtyError, PtyExitStatus, PtySize, SpawnCommand};
pub use portable::PortablePtyBackend;
pub use pump::pump_reader;
pub use session::{PtyKiller, PtyResizer, PtySession};

#[cfg(any(test, feature = "testing"))]
pub use fake::{FakeExit, FakeProbe, FakePtyBackend, FakeScript};
