//! UI components. Each submodule builds one part of the DAP shell and the
//! behaviour that belongs to it; `main.rs` assembles them into the window.

pub(crate) mod library;
pub(crate) mod mini;
pub(crate) mod now_playing;
pub(crate) mod queue;
pub(crate) mod search;
pub(crate) mod settings;
