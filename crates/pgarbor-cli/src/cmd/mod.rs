//! Command implementations. One module per command; each exposes `run(ctx, args) -> CliResult<Exit>`.
//! `mod.rs` is a frozen index, not leased.
pub mod branch;
pub mod compact;
pub mod doctor;
pub mod dump;
pub mod dump_data;
pub mod exec;
pub mod import;
pub mod init;
pub mod inspect;
pub mod serve;
pub mod shell;
pub mod shell_meta;
pub mod status;
pub mod verify;
pub mod version;
