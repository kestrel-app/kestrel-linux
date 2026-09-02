//! Video ingest and playback.

pub mod player;
pub mod stream;

pub use player::PlaybackWorker;
pub mod audio;

// Re-exported as this module's interface. The binary itself does not use
// every name, and in a crate with no external consumers that reads as an
// unused import - but the tests do, and cargo fix will happily delete them
// and break the test build, which is how this comment came to exist.
#[allow(unused_imports)]
pub use stream::{Frame, Retirer, StreamState, StreamStats, StreamView, StreamWorker};
