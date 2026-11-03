//! VMIDs and PASIDs.
//!
//! A VMID is a hardware context slot of one hub, handed out per job at submission time (amdgpu_ids.c:397
//! amdgpu_vmid_grab): reuse the slot this VM had if its page directory still matches (:334 grab_used), else
//! take the least recently used idle one (:199 grab_idle), else the job waits. So any number of VMs share a
//! few slots. On the 780M graphics owns VMIDs 1..7 per hub and MES/KFD 8..15 (gmc_v11_0.c:854,
//! first_kfd_vmid = 8); VMID 0 is the kernel's GART context. The range is a constructor argument.
//!
//! A PASID names a process's address space to the IOMMU and in fault interrupts (amdgpu_ids.c:60).

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use crate::errno;

/// Unique token of one VM (never reused for another VM: a recycled token with a recycled root table at the
/// same address would skip a needed TLB flush).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner(pub u64);

/// A VMID for one job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grab {
    pub vmid: u32,
    /// The job must load the page-table base and invalidate this VMID's TLB before it runs.
    pub needs_flush: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdError {
    /// Empty or overflowing VMID range, or VMID 0 in it.
    BadRange,
    /// Not a VMID of this manager.
    NotOurs(u32),
    /// `job_done` without a running job on that VMID.
    NotBusy(u32),
    /// A reserved VMID was asked for but none is reserved (or nothing is left to reserve).
    NoReserved,
    /// PASID width 0 (amdgpu_pasid_alloc: -EINVAL).
    BadBits,
    /// Every PASID of the width is taken (-ENOSPC).
    NoPasid,
    /// Freeing a PASID that is not allocated.
    NotAllocated(u32),
}

impl IdError {
    pub fn errno(&self) -> i32 {
        match self {
            IdError::NoPasid => errno::ENOSPC,
            IdError::NoReserved => errno::EBUSY,
            _ => errno::EINVAL,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    owner: Option<Owner>,
    pd: u64,
    flushed: u64,
    active: u32,
}

/// VMIDs of one hub.
pub struct VmidMgr {
    first: u32,
    slots: Vec<Slot>,
    /// Slot indexes, least recently used first; the reserved slot is not in it.
    lru: Vec<usize>,
    reserved: Option<usize>,
    reserved_users: u32,
}

impl VmidMgr {
    /// VMIDs `first .. first + count`.
    pub fn new(first: u32, count: u32) -> Result<VmidMgr, IdError> {
        if first == 0 || count == 0 || first.checked_add(count).is_none() {
            return Err(IdError::BadRange);
        }
        let n = count as usize;
        let slot = Slot { owner: None, pd: 0, flushed: 0, active: 0 };
        Ok(VmidMgr { first, slots: alloc::vec![slot; n], lru: (0..n).collect(), reserved: None, reserved_users: 0 })
    }

    fn slot_of(&self, vmid: u32) -> Result<usize, IdError> {
        let i = vmid.checked_sub(self.first).ok_or(IdError::NotOurs(vmid))? as usize;
        if i < self.slots.len() { Ok(i) } else { Err(IdError::NotOurs(vmid)) }
    }

    /// A VMID for a job of VM `owner` with root `pd` (its page-table base) whose tables are at TLB sequence
    /// `seq`. `reserved`: the VM holds the reserved VMID (AMDGPU_VM_OP_RESERVE_VMID). `Ok(None)`: every VMID
    /// is busy, wait for a job to finish.
    pub fn grab(&mut self, owner: Owner, pd: u64, seq: u64, reserved: bool) -> Result<Option<Grab>, IdError> {
        if reserved {
            let i = self.reserved.ok_or(IdError::NoReserved)?;
            let s = &self.slots[i];
            let stale = s.owner != Some(owner) || s.pd != pd;
            if stale && s.active > 0 {
                return Ok(None);
            }
            let needs_flush = stale || s.flushed < seq;
            return Ok(Some(self.take(i, owner, pd, seq, needs_flush)));
        }
        let used = self.lru.iter().rev().copied().find(|&i| self.slots[i].owner == Some(owner) && self.slots[i].pd == pd);
        if let Some(i) = used {
            let needs_flush = self.slots[i].flushed < seq;
            return Ok(Some(self.take(i, owner, pd, seq, needs_flush)));
        }
        let idle = self.lru.iter().copied().find(|&i| self.slots[i].active == 0);
        Ok(idle.map(|i| self.take(i, owner, pd, seq, true)))
    }

    fn take(&mut self, i: usize, owner: Owner, pd: u64, seq: u64, needs_flush: bool) -> Grab {
        let s = &mut self.slots[i];
        s.owner = Some(owner);
        s.pd = pd;
        s.active += 1;
        if needs_flush {
            s.flushed = seq;
        }
        if let Some(p) = self.lru.iter().position(|&j| j == i) {
            self.lru.remove(p);
            self.lru.push(i);
        }
        Grab { vmid: self.first + i as u32, needs_flush }
    }

    /// The job that grabbed `vmid` has finished on the GPU.
    pub fn job_done(&mut self, vmid: u32) -> Result<(), IdError> {
        let i = self.slot_of(vmid)?;
        let s = &mut self.slots[i];
        s.active = s.active.checked_sub(1).ok_or(IdError::NotBusy(vmid))?;
        Ok(())
    }

    /// Jobs running on `vmid`.
    pub fn active(&self, vmid: u32) -> Result<u32, IdError> {
        Ok(self.slots[self.slot_of(vmid)?].active)
    }

    /// A VM is gone: its VMIDs forget it, so the next user flushes.
    pub fn forget(&mut self, owner: Owner) {
        for s in self.slots.iter_mut().filter(|s| s.owner == Some(owner)) {
            s.owner = None;
            s.pd = 0;
        }
    }

    /// After a GPU reset every VMID needs a flush (amdgpu_ids.c:532 amdgpu_vmid_reset_all).
    pub fn reset_all(&mut self) {
        self.slots.iter_mut().for_each(|s| {
            s.owner = None;
            s.pd = 0;
        });
    }

    /// Takes one VMID out of round robin for VMs that ask for a reserved one; all of them share it, counted
    /// (amdgpu_ids.c:462 amdgpu_vmid_alloc_reserved). Returns the VMID.
    pub fn reserve(&mut self) -> Result<u32, IdError> {
        if self.reserved.is_none() {
            if self.lru.is_empty() {
                return Err(IdError::NoReserved);
            }
            self.reserved = Some(self.lru.remove(0));
        }
        self.reserved_users += 1;
        Ok(self.first + self.reserved.unwrap_or(0) as u32)
    }

    /// Drops one user of the reserved VMID; the last one gives it back (amdgpu_ids.c:484).
    pub fn unreserve(&mut self) -> Result<(), IdError> {
        self.reserved_users = self.reserved_users.checked_sub(1).ok_or(IdError::NoReserved)?;
        if self.reserved_users == 0
            && let Some(i) = self.reserved.take() {
                self.lru.insert(0, i);
            }
        Ok(())
    }
}

/// PASIDs (amdgpu_ids.c:60 amdgpu_pasid_alloc): the lowest free value of the widest range
/// `[2^(bits-1), 2^bits)` that has one, narrowing on exhaustion; never 0.
#[derive(Default)]
pub struct PasidAlloc {
    used: BTreeSet<u32>,
}

impl PasidAlloc {
    pub fn new() -> PasidAlloc {
        PasidAlloc::default()
    }

    pub fn alloc(&mut self, bits: u32) -> Result<u32, IdError> {
        if bits == 0 {
            return Err(IdError::BadBits);
        }
        for b in (1..=bits.min(31)).rev() {
            let (lo, hi) = (1u32 << (b - 1), 1u32 << b);
            let mut want = lo;
            for &u in self.used.range(lo..hi) {
                if u != want {
                    break;
                }
                want += 1;
            }
            if want < hi {
                self.used.insert(want);
                return Ok(want);
            }
        }
        Err(IdError::NoPasid)
    }

    pub fn free(&mut self, pasid: u32) -> Result<(), IdError> {
        if self.used.remove(&pasid) { Ok(()) } else { Err(IdError::NotAllocated(pasid)) }
    }

    pub fn in_use(&self) -> usize {
        self.used.len()
    }
}
