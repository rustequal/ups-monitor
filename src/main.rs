//! The binary. Everything it does is in the library beside it.
//!
//! Split from that library so the module tree has a crate root that is not
//! also an entry point: a `[[bin]]`-only crate exports nothing, so the lints
//! that check a public surface have nothing to look at, and `main` is a poor
//! place to keep an architecture's worth of documentation.

// Set on the binary, where it belongs: the attribute chooses the Windows
// subsystem for this executable, and a library has no subsystem to choose.
// Debug builds keep the console so `evlog` output and panics are visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    ups_monitor::run();
}
