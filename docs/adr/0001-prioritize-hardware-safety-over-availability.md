# Prioritize hardware safety over availability

The daemon runs on an unattended remote compute box (KVM-accessible but not physically supervised) cooling Tesla P100 cards that have **no internal cooling whatsoever** — fans are mounted on a 3D-printed adaptor and driven from motherboard PWM headers. A silent cooling failure cooks the card in minutes; a spurious reboot only loses an in-flight job.

We therefore design every fault response around "fail loud, fail safe, fail fast": on any unrecoverable cooling fault we max all fans and let the kernel hardware watchdog hard-reboot the box. We deliberately reject softer policies (alert-only, partial fan-up, GPU power-cap) because they all assume someone is watching, and nobody is.

## Consequences

- The hardware watchdog (`/dev/watchdog` or `softdog`) is a hard requirement, not an optional feature.
- Every fault path must converge on "stop feeding the watchdog" — no fault may be silently swallowed.
- The daemon is single-purpose: it is *not* a general fan-control daemon and is not expected to coexist nicely with workstations or boxes running other critical workloads.
