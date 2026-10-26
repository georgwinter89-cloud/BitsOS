//! Registries by linker section ("distributed slices"): lanes register syscalls, MCP tools, ktests, device
//! nodes, PCI drivers and init calls in their own crates, so nobody edits a shared table (V2-PLAN §0b, §9.1).
//!
//! Mechanism: every entry is a `static` of the registry's entry type placed in the ELF section `kreg_<name>`;
//! the linker (kernel/linker.ld, or the host linker's automatic `__start_`/`__stop_` symbols in host tests)
//! makes the section contiguous, and `Registry::entries()` returns it as a slice. All entries of one registry
//! have the same type, so the section is a plain array.
//!
//! Trusted base: building the slice from two linker symbols is the one unsafe operation here.
#![no_std]
#![feature(allow_internal_unsafe)]
#![allow(internal_features)]

/// A registry over the linker section `kreg_<name>` holding entries of type `T`.
pub struct Registry<T: 'static> {
    pub name: &'static str,
    start: *const T,
    stop: *const T,
}

// SAFETY: a Registry only hands out shared references to `'static` entries that are themselves `Sync`.
unsafe impl<T: Sync> Sync for Registry<T> {}

impl<T: 'static> Registry<T> {
    #[doc(hidden)]
    pub const fn __new(name: &'static str, start: *const T, stop: *const T) -> Self {
        Registry { name, start, stop }
    }

    /// All registered entries, in link order (not meaningful; sort by a key if order matters).
    pub fn entries(&self) -> &'static [T] {
        let size = core::mem::size_of::<T>();
        if size == 0 {
            return &[];
        }
        let bytes = (self.stop as usize).saturating_sub(self.start as usize);
        let len = bytes / size;
        if len == 0 {
            return &[];
        }
        // SAFETY: start/stop are the linker's bounds of the section that only contains `#[used]` statics of
        // type T placed by `register!` (same size and alignment each), so [start, stop) is an array of T
        // that lives for the whole program.
        unsafe { core::slice::from_raw_parts(self.start, len) }
    }

    pub fn len(&self) -> usize {
        self.entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }
}

/// Declares a registry: `kreg::declare!(pub KTESTS: KtestEntry = "ktest");` gives a `pub static KTESTS`
/// over the section `kreg_ktest`. The section name must be a C identifier.
#[macro_export]
#[allow_internal_unsafe]
macro_rules! declare {
    ($vis:vis $NAME:ident : $T:ty = $sect:literal) => {
        $vis static $NAME: $crate::Registry<$T> = {
            #[allow(improper_ctypes)]
            unsafe extern "C" {
                #[link_name = concat!("__start_kreg_", $sect)]
                static START: $T;
                #[link_name = concat!("__stop_kreg_", $sect)]
                static STOP: $T;
            }
            $crate::Registry::__new($sect, &raw const START, &raw const STOP)
        };
    };
}

/// Places one entry into a registry's section. Used through the owners' macros (`ktest!`, `syscall!`, ...),
/// which also fix the entry type. Usable from `#![forbid(unsafe_code)]` crates.
#[macro_export]
#[allow_internal_unsafe]
macro_rules! register {
    ($sect:literal, $T:ty, $value:expr) => {
        const _: () = {
            #[used]
            #[unsafe(link_section = concat!("kreg_", $sect))]
            static ENTRY: $T = $value;
        };
    };
}

/// Init levels, run in this order by the kernel after the early console (V2-PLAN §9.1 point 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// CPU, memory, interrupts: things everything else needs.
    Core = 0,
    /// Subsystems without devices (VFS, network stack, scheduler services).
    Subsys = 1,
    /// Bus probing and drivers.
    Device = 2,
    /// Services on top of devices (MCP, console, mirror).
    Service = 3,
    /// Last: start user space.
    Late = 4,
}

pub struct InitCall {
    pub name: &'static str,
    pub level: Level,
    pub func: fn() -> Result<(), &'static str>,
}

declare!(pub INITCALLS: InitCall = "initcall");

/// `kreg::initcall!(Level::Subsys, "vfs", vfs::init);`
#[macro_export]
macro_rules! initcall {
    ($level:expr, $name:literal, $func:path) => {
        $crate::register!("initcall", $crate::InitCall, $crate::InitCall { name: $name, level: $level, func: $func });
    };
}

/// Runs every init call of `level` in name order (deterministic), returning the failures.
pub fn run_level(level: Level, mut on_result: impl FnMut(&'static str, Result<(), &'static str>)) {
    let all = INITCALLS.entries();
    // Small fixed-size scratch would be a limit; instead walk names in sorted order without allocating.
    let mut last: Option<&str> = None;
    loop {
        let next = all
            .iter()
            .filter(|c| c.level == level && last.is_none_or(|l| c.name > l))
            .min_by(|a, b| a.name.cmp(b.name));
        let Some(c) = next else { break };
        on_result(c.name, (c.func)());
        last = Some(c.name);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    pub struct Demo {
        pub id: u32,
    }
    declare!(DEMO: Demo = "kreg_selftest");
    register!("kreg_selftest", Demo, Demo { id: 7 });
    register!("kreg_selftest", Demo, Demo { id: 9 });

    #[test]
    fn registry_collects_entries() {
        let mut ids: std::vec::Vec<u32> = DEMO.entries().iter().map(|d| d.id).collect();
        ids.sort();
        assert_eq!(ids, [7, 9]);
    }

    fn ok() -> Result<(), &'static str> {
        Ok(())
    }
    initcall!(Level::Late, "b-test", ok);
    initcall!(Level::Late, "a-test", ok);

    #[test]
    fn init_levels_run_sorted() {
        let mut seen = std::vec::Vec::new();
        run_level(Level::Late, |n, r| {
            assert!(r.is_ok());
            seen.push(n)
        });
        assert_eq!(seen, ["a-test", "b-test"]);
    }
}
