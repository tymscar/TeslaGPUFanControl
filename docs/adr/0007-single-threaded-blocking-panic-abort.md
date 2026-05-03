# Single-threaded blocking execution; `panic = abort`

The daemon runs in a single thread, doing blocking I/O. There is no async runtime, no thread pool, no channels. The main loop is `loop { run_one_poll(); thread::sleep(poll_interval_ms); }`.

`Cargo.toml` sets `panic = "abort"` for the release profile: any panic terminates the process immediately without unwinding. Under `Restart=always` + `WatchdogSec=30s` + `/dev/watchdog`, this is the correct behavior — a panic represents an invariant violation we cannot recover from, which is the same threat surface as an unhandled fault (ADR-0001), and the recovery is identical: systemd restarts the daemon, the kernel watchdog catches deeper hangs.

## Why not async (tokio / async-std)

Workload per poll: ~10 ms of work, ~990 ms of sleep, no concurrent I/O. tokio would add ~200 transitive dependencies, async coloring throughout the codebase, and a runtime that masks panics inside tasks unless explicitly configured. Zero concurrency benefit because there is no concurrency.

## Why not threads

There is nothing to parallelize. Even `--calibrate-fans` is sequential by design (parallel sweeps would corrupt RPM readings via shared chip state). Threads would add synchronization primitives we don't need, and a panic in one thread leaves the others in undefined partial state — worse fault behavior, not better.

## Why blocking is fine

Sysfs reads/writes are kernel-side memory accesses, single-digit microseconds. NVML temperature reads are ~1 ms each. Blocking the only thread for the total ~10 ms of work per poll is invisible — the daemon spends 99% of its time in `nanosleep`, which is exactly what we want.

## Signal handling under single-threaded blocking

Signals interrupt the active syscall. `thread::sleep` (which calls `nanosleep` underneath) returns early on signal delivery (`EINTR`); the signal handler flips an `AtomicBool` and the main loop checks it after every wake. Clean, no race conditions, no thread coordination.

## Consequences

- `Cargo.toml` does not depend on `tokio`, `async-std`, `crossbeam`, `rayon`, or any concurrency primitives beyond `std`.
- `main.rs` is small and linear — the entire program flow fits on one screen.
- Any future feature that genuinely requires concurrency (e.g. a web monitoring endpoint that must respond while the poll loop is sleeping) requires re-evaluating this ADR rather than bolting on `tokio` opportunistically.
- `unsafe` is allowed only in the watchdog ioctl wrapper, which uses `nix::ioctl_*` macros.

## Lint and panic policy (paired with execution model)

- `clippy::unwrap_used = "deny"` — `.unwrap()` is forbidden outside `#[cfg(test)]`.
- `clippy::expect_used = "warn"` — `.expect("...")` is allowed only in `main.rs` initialization, and the message must name the invariant being asserted (e.g. `expect("/dev/watchdog open — see ADR-0001 for why this is fatal")`).
- `clippy::indexing_slicing = "warn"` — prefer `.get()` on slices.
- `clippy::panic = "warn"` — explicit `panic!()` calls require a comment explaining why the invariant is unrecoverable.
- Module-level error types via `thiserror`; cross-cutting fatal errors at the top of `main` via `anyhow`. No mixing.
