//! Windows-specific plumbing, kept behind one module so the rest of the host
//! stays platform-neutral and easy to reason about.

pub mod admin_task;
pub mod autostart;
pub mod elevate;
pub mod message_window;
pub mod power;
pub mod single_instance;
pub mod tray;
