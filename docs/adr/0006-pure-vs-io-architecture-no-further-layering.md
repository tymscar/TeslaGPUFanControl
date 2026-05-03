# Architecture: pure-vs-I/O split, no enterprise layering

The codebase is partitioned along exactly one architectural axis: **pure modules vs I/O modules**. Pure modules never import from I/O modules; the dependency rule is enforced by code review (and naturally by Rust's module visibility, since pure modules don't need any types from `nix`, `std::fs`, `nvml-wrapper`, etc.).

| Pure | I/O |
|---|---|
| `units.rs`, `curve.rs`, `group.rs`, `watchdog::FaultTracker`, `config::validation` | `nvml.rs`, `chip.rs`, `fan.rs`, `watchdog::HardwareWatchdog`, `logger.rs` |

`main.rs` is the composition root and is allowed to import from both sides; it is the only module that does.

We **explicitly reject** further architectural layering — no ports/adapters, no hexagonal, no use-case interactors, no entity/use-case/interface-adapter concentric circles. The canonical "Clean Architecture" (Robert C. Martin) is designed for enterprise apps with multiple delivery mechanisms, complex business rules, and large teams. This project has one delivery mechanism (systemd-managed daemon), domain logic that is entirely about timing and I/O coordination, and a maintainer count of one. Applying enterprise layering would add abstraction surface area without buying anything testable, replaceable, or comprehensible.

## Why this single split is enough

- **Testability** — the pure modules cover every interesting logical decision (curve interpolation, group max-of-curves, fault counter state machine, config validation). All testable as pure functions, no mocks, no fixtures, no harness.
- **Replaceability** — when a contributor wants to swap NVML for DCGM, or sysfs for a different hwmon path, the change is contained to one I/O module. The pure side doesn't move.
- **Comprehensibility** — a future reader can pick up the project by reading the pure modules first (the *what*) and then the I/O modules (the *how*). The dependency direction is the reading order.

## Consequences

- A new module is classified as pure or I/O at creation time. If it's tempted to be both, split it.
- I/O modules expose small, intention-revealing APIs (`Fan::take_manual_control()`, `Nvml::read_temp(idx)`) — not generic abstractions. We do not pre-introduce traits "for testability" unless a real second implementation exists or is imminent.
- The single trait we *do* introduce — `TempReader` for NVML in `nvml.rs` — exists because there's a concrete second implementation (`FakeNvml` for tests) and no other way to test the main loop without a GPU. This is the trait threshold: real second implementation, or no trait.
- Pure modules unit-test in-module via `#[cfg(test)] mod tests`. I/O modules test against `tempfile`-backed fake sysfs (`fan.rs`, `chip.rs`), against `softdog` for real ioctl coverage (`watchdog.rs`), or via the `TempReader` trait (`nvml.rs`).
