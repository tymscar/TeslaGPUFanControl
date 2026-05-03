//! tesla_fan_control — Wave 1 scaffold.
//!
//! The full daemon (init order, signal handlers, main loop) lands in Wave 3.
//! Wave 1 establishes the Cargo project, all pure-logic modules, and stubs
//! for the I/O modules so `cargo build` compiles end-to-end.

// Wave 1 modules expose their full public API for Wave 2/3 consumers; they
// are exercised by tests but not yet wired into the daemon's main loop.
// Re-evaluate this when Wave 3 lands; if any symbols remain unused then,
// drop them.
#![allow(dead_code)]

mod chip;
mod config;
mod curve;
mod fan;
mod group;
mod logger;
mod nvml;
mod units;
mod watchdog;

fn main() {
    eprintln!("tesla_fan_control v0.1.0 — Wave 1 scaffold (daemon impl pending Wave 3)");
    std::process::exit(0);
}
