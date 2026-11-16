//! The part of osh that needs no terminal: the OpenShell CLI wrappers, host facts, policy and
//! profile logic. It must not name `ratatui`, `libghostty_vt` or `portable_pty`, so it builds
//! and tests without Zig and can back a headless front end.

pub mod agents;
pub mod assets;
pub mod cli;
pub mod ghrepo;
pub mod mcp;
pub mod ops;
pub mod platform;
pub mod preload;
pub mod sandbox;
pub mod settings;
pub mod testmode;
pub mod text;
pub mod watch;
