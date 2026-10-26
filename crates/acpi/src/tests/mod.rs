//! Host tests (CP-2). `crate::testkit` makes tables; `good` checks a Mini-PC-like set; `bad` breaks one thing at a
//! time; `hostile` throws mutated and random bytes at every parser; `guard` checks the register guard;
//! `bounds` checks that hostile tables cannot make the work or the notes grow without bound; `irq` (CP-3) checks the
//! interrupt routing plan, NMI pin choice, I/O APIC entries and the storm guard.

mod bad;
mod bounds;
mod good;
mod guard;
mod hostile;
mod irq;
