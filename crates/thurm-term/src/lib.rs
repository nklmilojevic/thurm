//! Thurm terminal engine: libghostty-vt plus kitty graphics, OSC extensions (cwd, program status,
//! notifications, shell-integration marks), input encoding and frame generation.

pub mod filter;
pub mod keys;
pub mod kitty;
pub mod mode;
pub mod osc;
pub mod placeholder;
pub mod program_status;
pub mod terminal;
pub mod vt;

pub use mode::TermMode;
pub use terminal::{ClientView, EngineConfig, MouseOutcome, Snapshot, TermEvent, Terminal};
