//! The NovaOS port of `edelib` — the small support library EDE builds on.
//!
//! edelib exists because FLTK deliberately lacks what a desktop needs:
//! logging, configuration storage, `.desktop` files, an icon theme, and an
//! application bootstrap. This crate is the same library, rewritten in Rust
//! for NovaOS, with the same shape and a host-testable design.
//!
//! # Licence
//!
//! edelib is LGPL-2.0. This port is LGPL-2.0 as well; see the workspace
//! licence file. NovaOS's own crates (the desktop, the panel, the window
//! manager) are GPL-2.0, matching EDE.
//!
//! # What is here
//!
//! - [`log`] — the levelled logger every EDE application installs on boot.
//! - [`config`] — the sectioned key/value store behind `~/.ede/*.conf`.
//! - [`desktop_file`] — a parser for the freedesktop `.desktop` files EDE
//!   keys its launcher, desktop icons, and autostart off.
//! - [`app`] — the application base: version, name, logging init, and the
//!   conventional config directory path.
//!
//! Everything is `no_std`, allocation-free, and compiles for the host, so
//! the parser and the store run their tests on every `cargo test` run —
//! not only when a desktop boots in QEMU.

#![no_std]
#![forbid(unsafe_code)]

pub mod app;
pub mod config;
pub mod desktop_file;
pub mod log;

pub use app::EdeApplication;
pub use config::EdeConfig;
pub use desktop_file::{DesktopFile, ExecArgs};
pub use log::{EdeLog, Level};

/// The conventional directory holding a user's EDE configuration, relative
/// to the user's home directory.
///
/// EDE keeps its settings in `~/.ede/`; NovaOS follows the same layout so
/// the mental model carries over unchanged.
pub const EDE_DIR: &str = ".ede";

/// The file inside [`EDE_DIR`] holding desktop settings.
pub const DESKTOP_CONF: &str = "desktop.conf";

/// The file inside [`EDE_DIR`] holding panel settings.
pub const PANEL_CONF: &str = "panel.conf";

/// The file inside [`EDE_DIR`] holding the desktop icon layout.
pub const ICONS_CONF: &str = "icons.conf";

/// The file inside [`EDE_DIR`] holding window-manager preferences.
pub const WM_CONF: &str = "wm.conf";
