//! A recording machine for the host tests and the ktests: the fake `_CST` path runs `enter` against it.

use super::entry::IdleHw;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    In8(u16),
    In16(u16),
    Out16(u16, u16),
    In32(u16),
    Halt,
    Sti,
}

#[derive(Default)]
pub struct Recorder {
    pub ops: RefCell<Vec<Op>>,
    /// What a PM1 status read returns.
    pub pm1: Cell<u16>,
    /// A C-state port the guard refuses (its read does not happen).
    pub refuse: Cell<Option<u16>>,
}

impl Recorder {
    pub fn take(&self) -> Vec<Op> {
        core::mem::take(&mut *self.ops.borrow_mut())
    }
}

impl IdleHw for Recorder {
    fn read8(&self, port: u16) -> bool {
        if self.refuse.get() == Some(port) {
            return false;
        }
        self.ops.borrow_mut().push(Op::In8(port));
        true
    }
    fn read16(&self, port: u16) -> u16 {
        self.ops.borrow_mut().push(Op::In16(port));
        self.pm1.get()
    }
    fn write16(&self, port: u16, v: u16) {
        self.ops.borrow_mut().push(Op::Out16(port, v));
        // Write 1 to clear.
        self.pm1.set(self.pm1.get() & !v);
    }
    fn read32(&self, port: u16) {
        self.ops.borrow_mut().push(Op::In32(port));
    }
    fn halt(&self) {
        self.ops.borrow_mut().push(Op::Halt);
    }
    fn sti(&self) {
        self.ops.borrow_mut().push(Op::Sti);
    }
}
