//! Web browsers, like IDEA's "Open In › Browser" and the editor's browser bar.
//!
//! [`WebServer`] is the built-in server: it serves project files read-only on
//! `127.0.0.1`, so pages opened from the IDE load relative scripts, styles and
//! ES modules the way they would from a real server. Every URL starts with a
//! random per-run token, and requests whose `Host` is not the server itself are
//! refused (DNS rebinding). [`installed`] lists the browsers found on this
//! machine and [`open`] launches one, or the system default.

mod browsers;
mod server;

pub use browsers::{Browser, installed, open};
pub use server::WebServer;
