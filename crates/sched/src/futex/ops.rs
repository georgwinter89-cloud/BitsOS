//! The two-futex operations and futex_waitv: FUTEX_REQUEUE / FUTEX_CMP_REQUEUE (linux-6.6/kernel/futex/
//! requeue.c:364, non-PI), FUTEX_WAKE_OP (waitwake.c:236 with `futex_atomic_op_inuser` :186 and x86's
//! `arch_futex_atomic_op_inuser`), futex_waitv (waitwake.c:396-544, syscalls.c:273).

use super::table::{self, Key};
use super::{KeyRef, Space, Table, buckets, cancel_block, expired, me, wake_entry};
use super::{Deadline, REQUEUED, TIMEOUTS, WAITS, begin, enqueue, not_loaded, retry_fault, sleep, unqueue};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::user_word::{WordError, WordOp};
use uapi::errno::{EAGAIN, EFAULT, EINVAL, ENOMEM, ENOSYS, ETIMEDOUT};

/// FUTEX_REQUEUE (`cmp` None) and FUTEX_CMP_REQUEUE: wakes `nr_wake` waiters of `a1` and moves up to
/// `nr_requeue` more to `a2`. EINVAL for negative counts, EAGAIN when `*a1 != cmp` (read under both bucket
/// locks). Returns woken + moved.
pub fn requeue(sp: &dyn Space, a1: u64, a2: u64, shared: bool, nr_wake: i32, nr_requeue: i32, cmp: Option<u32>) -> Result<i64, i64> {
    if nr_wake < 0 || nr_requeue < 0 {
        return Err(EINVAL);
    }
    let t = buckets()?;
    let mut faults = 0;
    'retry: loop {
        let k1 = sp.key(a1, shared, false)?;
        let k2 = sp.key(a2, shared, false)?;
        let (i1, i2) = (t.index(&k1.key), t.index(&k2.key));
        loop {
            let (mut q1, mut q2) = t.lock2(i1, i2);
            if let Some(c) = cmp {
                match sp.word(a1, WordOp::Load) {
                    Ok(v) if v != c => return Err(EAGAIN),
                    Ok(_) => {}
                    Err(WordError::NotLoaded) => {
                        drop((q1, q2));
                        not_loaded();
                        return Err(EFAULT);
                    }
                    Err(_) => {
                        drop((q1, q2));
                        sp.read_u32(a1)?;
                        retry_fault(&mut faults)?;
                        if shared {
                            continue 'retry;
                        }
                        continue;
                    }
                }
            }
            if let Some(q2) = q2.as_deref_mut() {
                let cap = (nr_wake as usize).saturating_add(nr_requeue as usize);
                let need = table::count(&q1, &k1.key, cap);
                if q2.try_reserve(need).is_err() {
                    return Err(ENOMEM);
                }
            }
            let (mut moved, k2key) = (0u64, k2.key);
            let n = table::requeue(&mut q1, q2.as_deref_mut(), &k1.key, &k2key, nr_wake, nr_requeue, &mut wake_entry, &mut |e| {
                e.who.futex.set_loc(e.slot, i2);
                moved += 1;
            });
            drop((q1, q2));
            REQUEUED.add(moved);
            return n;
        }
    }
}

/// The decoded FUTEX_WAKE_OP operation word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodedOp {
    pub op: u32,
    pub cmp: u32,
    pub oparg: i32,
    pub cmparg: i32,
    /// FUTEX_OP_OPARG_SHIFT with an out-of-range shift (Linux masks it and logs).
    pub shift_fixed: bool,
}

/// `futex_atomic_op_inuser`'s decoding (sign-extended 12-bit arguments, the shift flag in bit 31).
pub fn decode(encoded: u32) -> EncodedOp {
    let op = (encoded & 0x7000_0000) >> 28;
    let cmp = (encoded & 0x0f00_0000) >> 24;
    let sext = |v: u32| ((v << 20) as i32) >> 20;
    let mut oparg = sext((encoded & 0x00ff_f000) >> 12);
    let cmparg = sext(encoded & 0x0000_0fff);
    let mut shift_fixed = false;
    if encoded & (8 << 28) != 0 {
        if !(0..=31).contains(&oparg) {
            shift_fixed = true;
            oparg &= 31;
        }
        oparg = 1i32 << oparg;
    }
    EncodedOp { op, cmp, oparg, cmparg, shift_fixed }
}

/// The operation for `ksync::user_word` (None: not an operation x86 knows, ENOSYS).
pub fn word_op(e: &EncodedOp) -> Option<WordOp> {
    let a = e.oparg as u32;
    Some(match e.op {
        0 => WordOp::Set(a),
        1 => WordOp::Add(a),
        2 => WordOp::Or(a),
        3 => WordOp::AndN(a),
        4 => WordOp::Xor(a),
        _ => return None,
    })
}

/// The comparison of the old value (None: unknown comparison, ENOSYS after the operation was applied).
pub fn compare(e: &EncodedOp, old: u32) -> Option<bool> {
    let o = old as i32;
    Some(match e.cmp {
        0 => o == e.cmparg,
        1 => o != e.cmparg,
        2 => o < e.cmparg,
        3 => o >= e.cmparg,
        4 => o <= e.cmparg,
        5 => o > e.cmparg,
        _ => return None,
    })
}

static SHIFT_SAID: AtomicBool = AtomicBool::new(false);

/// FUTEX_WAKE_OP: applies the encoded operation to `*a2` atomically (both bucket locks held), wakes up to
/// `nr_wake` waiters of `a1` and, when the comparison of the old `*a2` holds, up to `nr_wake2` of `a2`.
pub fn wake_op(sp: &dyn Space, a1: u64, a2: u64, shared: bool, nr_wake: i32, nr_wake2: i32, encoded: u32) -> Result<i64, i64> {
    let e = decode(encoded);
    if e.shift_fixed && !SHIFT_SAID.swap(true, Ordering::Relaxed) {
        // Linux: pr_info_ratelimited("futex_wake_op: %s tries to shift op by %d; fix this program").
        klog::event!(Info, "futex", "wake_op.shift_masked", thread = crate::current_id(); "a program shifted FUTEX_OP_OPARG_SHIFT out of 0..31 (masked, as Linux)");
    }
    let t = buckets()?;
    let mut faults = 0;
    'retry: loop {
        let k1 = sp.key(a1, shared, false)?;
        let k2 = sp.key(a2, shared, true)?;
        let (i1, i2) = (t.index(&k1.key), t.index(&k2.key));
        loop {
            let (mut q1, mut q2) = t.lock2(i1, i2);
            let Some(op) = word_op(&e) else {
                return Err(ENOSYS);
            };
            let old = match sp.word(a2, op) {
                Ok(v) => v,
                Err(WordError::NotLoaded) => {
                    drop((q1, q2));
                    not_loaded();
                    return Err(EFAULT);
                }
                Err(_) => {
                    drop((q1, q2));
                    sp.fault_in(a2, true)?;
                    retry_fault(&mut faults)?;
                    if shared {
                        continue 'retry;
                    }
                    continue;
                }
            };
            let Some(hit) = compare(&e, old) else {
                return Err(ENOSYS);
            };
            // A PI waiter on either key: EINVAL (Linux's `goto out_unlock`, the ones before it stay woken).
            let mut ret = table::wake(&mut q1, &k1.key, nr_wake, super::BITSET_MATCH_ANY, &mut wake_entry)?;
            if hit {
                let q = match q2.as_deref_mut() {
                    Some(q) => q,
                    None => &mut *q1,
                };
                ret += table::wake(q, &k2.key, nr_wake2, super::BITSET_MATCH_ANY, &mut wake_entry)?;
            }
            drop((q1, q2));
            return Ok(ret);
        }
    }
}

/// One entry of a futex_waitv list (already parsed and checked by the syscall layer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitvItem {
    pub addr: u64,
    /// `(u32)waitv.val`.
    pub val: u32,
    pub shared: bool,
}

/// Takes slots 0..n off; the highest slot a waker had taken (Linux `unqueue_multiple`), or None.
fn unqueue_multiple(t: &Table, me: &alloc::sync::Arc<crate::Thread>, n: usize) -> Option<usize> {
    let mut ret = None;
    for i in 0..n {
        if !unqueue(t, me, i as u32) {
            ret = Some(i);
        }
    }
    ret
}

/// futex_waitv: sleeps until one of the words is woken; returns an index of a woken one (Linux returns the
/// highest woken index it finds), EAGAIN when a word differs from its value at setup, ETIMEDOUT, EFAULT, EINVAL
/// for a misaligned word, or `Space::interrupted`'s errno.
pub fn wait_multiple(sp: &dyn Space, items: &[WaitvItem], deadline: Option<Deadline>) -> Result<i64, i64> {
    let me = me()?;
    let t = buckets()?;
    me.futex.reserve(items.len())?;
    let mut keys: Vec<KeyRef> = Vec::new();
    keys.try_reserve_exact(items.len()).map_err(|_| ENOMEM)?;
    WAITS.inc();
    let mut faults = 0;
    loop {
        // futex_wait_multiple_setup: keys first (they may sleep), then queue one by one.
        let mut retry = false;
        'setup: loop {
            for (i, it) in items.iter().enumerate() {
                if !it.shared && retry {
                    continue;
                }
                let k = sp.key(it.addr, it.shared, false)?;
                if i < keys.len() {
                    keys[i] = k;
                } else {
                    keys.push(k);
                }
            }
            begin(&me);
            for (i, it) in items.iter().enumerate() {
                let key: Key = keys[i].key;
                let bi = t.index(&key);
                let mut q = t.lock(bi);
                let r = if q.try_reserve(1).is_err() { Err(None) } else { sp.word(it.addr, WordOp::Load).map_err(Some) };
                match r {
                    Ok(v) if v == it.val => {
                        enqueue(&mut q, bi, &me, key, super::BITSET_MATCH_ANY, i as u32);
                        continue;
                    }
                    _ => {}
                }
                drop(q);
                cancel_block(&me);
                // A futex woken meanwhile wins over any error.
                if let Some(w) = unqueue_multiple(t, &me, i) {
                    return Ok(w as i64);
                }
                match r {
                    Err(None) => return Err(ENOMEM),
                    Err(Some(WordError::NotLoaded)) => {
                        not_loaded();
                        return Err(EFAULT);
                    }
                    Err(Some(_)) => {
                        sp.read_u32(it.addr)?;
                        retry_fault(&mut faults)?;
                        retry = true;
                        continue 'setup;
                    }
                    Ok(_) => return Err(EAGAIN),
                }
            }
            break;
        }
        sleep(&me, sp, deadline);
        if let Some(w) = unqueue_multiple(t, &me, items.len()) {
            return Ok(w as i64);
        }
        if expired(deadline) {
            TIMEOUTS.inc();
            return Err(ETIMEDOUT);
        }
        if let Some(e) = sp.interrupted() {
            return Err(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(op: u32, cmp: u32, oparg: u32, cmparg: u32) -> u32 {
        // FUTEX_OP(op, oparg, cmp, cmparg) from the uapi header.
        ((op & 0xf) << 28) | ((cmp & 0xf) << 24) | ((oparg & 0xfff) << 12) | (cmparg & 0xfff)
    }

    #[test]
    fn decode_sign_extends_and_shifts() {
        let e = decode(enc(1, 2, 0xfff, 0x800));
        assert_eq!((e.op, e.cmp, e.oparg, e.cmparg, e.shift_fixed), (1, 2, -1, -2048, false));
        // FUTEX_OP_OPARG_SHIFT (8 in the op nibble): 1 << oparg.
        let e = decode(enc(8 | 2, 0, 5, 0));
        assert_eq!((e.op, e.oparg, e.shift_fixed), (2, 32, false));
        // Out of range: masked to 0..31 (and said once).
        let e = decode(enc(8, 0, 40, 0));
        assert_eq!((e.oparg, e.shift_fixed), (1 << 8, true));
        let e = decode(enc(8, 0, 0xfff, 0));
        assert_eq!((e.oparg, e.shift_fixed), (i32::MIN, true));
    }

    #[test]
    fn ops_and_comparisons_as_linux() {
        assert_eq!(word_op(&decode(enc(0, 0, 7, 0))), Some(WordOp::Set(7)));
        assert_eq!(word_op(&decode(enc(3, 0, 1, 0))), Some(WordOp::AndN(1)));
        assert_eq!(word_op(&decode(enc(5, 0, 1, 0))), None);
        assert_eq!(word_op(&decode(enc(7, 0, 1, 0))), None);
        let e = decode(enc(0, 2, 0, 0xfff)); // old < -1
        assert_eq!(compare(&e, (-2i32) as u32), Some(true));
        assert_eq!(compare(&e, 0), Some(false));
        let e = decode(enc(0, 5, 0, 3)); // old > 3
        assert_eq!(compare(&e, 4), Some(true));
        assert_eq!(compare(&decode(enc(0, 6, 0, 0)), 0), None);
        assert_eq!(compare(&decode(enc(0, 1, 0, 0)), 1), Some(true));
        assert_eq!(compare(&decode(enc(0, 3, 0, 1)), 1), Some(true));
        assert_eq!(compare(&decode(enc(0, 4, 0, 1)), 2), Some(false));
    }
}
