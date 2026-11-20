//! A card's socket table: smoltcp's `SocketSet` with its slots counted, so that it never grows under the
//! card's lock.
//!
//! smoltcp puts a new socket into the first empty slot and pushes a slot when there is none; that push grew
//! the table under `net.iface`, where the heap can neither reclaim nor fall back to single frames (16 slots
//! are 9 KiB; the detector of 12.11.2026 named it). Here the table is made with all its slots, and a larger
//! one is made with the lock dropped (`Room`, used like `ksync::Spare`) and takes the sockets over under the
//! lock, each to the handle it had. The lists that hold one entry per socket (`listen`, `orphans`) get their
//! room with the table: they never grow under the lock either.
//!
//! smoltcp looks for the first empty slot from the start for every socket it is given, so putting n sockets
//! into a table costs n²/2 slot looks. That part is done with no lock held: the larger table is made with a
//! filler in every slot the old one has (`Sockets::fill`), and under the lock each socket only changes places
//! with the filler of its own slot (`take_all`: one pass). plan/spec/net.md §10.2 has the numbers.
//!
//! A larger table needs memory, so it can be refused. Slots for the kept pool's sockets (the MCP reserve) are
//! therefore held back from everyone else (`Inner::admits`): programs get `NoMemory` from a table that cannot
//! grow, MCP's listening sockets still find their slots.

use super::iface::{Iface, Inner, ListenSock, Orphan};
use super::Stack;
use crate::NetError;
use alloc::vec::Vec;
use ksync::SpinLockGuard;
use smoltcp::iface::{SocketHandle, SocketSet, SocketStorage};
use smoltcp::socket::{dhcpv4, AnySocket, Socket};

/// Slots a card starts with (its DHCP socket, the MCP reserve, a few connections).
pub(crate) const FIRST_SLOTS: usize = 16;

pub(crate) struct Sockets {
    set: SocketSet<'static>,
    slots: usize,
    used: usize,
    /// Sockets of the kept pool (the MCP reserve) among them.
    kept: usize,
}

/// What stands in a slot while its socket is moved: no buffers, nothing on the heap.
fn filler() -> Socket<'static> {
    Socket::Dhcpv4(dhcpv4::Socket::new())
}

impl Sockets {
    /// A table of `slots` empty slots in `v`, which has the room for them.
    fn over(mut v: Vec<SocketStorage<'static>>, slots: usize) -> Self {
        debug_assert!(v.is_empty() && v.capacity() >= slots);
        v.resize_with(slots, || SocketStorage::EMPTY);
        Sockets { set: SocketSet::new(v), slots, used: 0, kept: 0 }
    }

    /// A new card's table (registration: memory is asked for like the card's other buffers).
    pub(crate) fn first() -> Self {
        Self::over(Vec::with_capacity(FIRST_SLOTS), FIRST_SLOTS)
    }

    pub(crate) fn slots(&self) -> usize {
        self.slots
    }
    pub(crate) fn used(&self) -> usize {
        self.used
    }
    pub(crate) fn free(&self) -> usize {
        self.slots - self.used
    }
    /// Sockets of the kept pool in the table.
    pub(crate) fn kept(&self) -> usize {
        self.kept
    }

    /// Into the first empty slot. The caller made sure there is one (`Iface::lock_room`); `kept`: a socket of
    /// the kept pool.
    pub(crate) fn add<T: AnySocket<'static>>(&mut self, socket: T, kept: bool) -> SocketHandle {
        debug_assert!(self.used < self.slots, "socket added without a free slot: smoltcp grows the table under the card's lock");
        if self.used == self.slots {
            // smoltcp pushes a slot (the old way; the detector reports it).
            self.slots += 1;
        }
        self.used += 1;
        self.kept += kept as usize;
        self.set.add(socket)
    }

    pub(crate) fn remove(&mut self, handle: SocketHandle, kept: bool) -> Socket<'static> {
        self.used -= 1;
        self.kept -= kept as usize;
        self.set.remove(handle)
    }

    pub(crate) fn get_mut<T: AnySocket<'static>>(&mut self, handle: SocketHandle) -> &mut T {
        self.set.get_mut(handle)
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (SocketHandle, &mut Socket<'static>)> {
        self.set.iter_mut()
    }

    /// For smoltcp's poll calls, which neither add nor remove sockets.
    pub(crate) fn smol(&mut self) -> &mut SocketSet<'static> {
        &mut self.set
    }

    /// A filler into each of the first `n` slots of a new table (no lock held: this is the n²/2 part). The
    /// fillers are not counted as sockets; `take_all` takes them out again.
    fn fill(&mut self, n: usize) {
        debug_assert!(self.used == 0 && n <= self.slots);
        for _ in 0..n {
            self.set.add(filler());
        }
    }

    /// Takes every socket of `old` over, each to the handle it had, in one pass; `self` is new with a filler
    /// in at least each of `old`'s slots (`fill`), `spare` has room for one handle per filler. `old` is left
    /// with the fillers where its sockets were.
    ///
    /// Both tables are walked in slot order, and a socket changes places with the filler that has its handle.
    /// A socket's smoltcp bookkeeping beside it (how long to stay quiet for a neighbour that does not answer
    /// ARP) stays behind with the slot and starts anew: one more egress try, which the interface's own ARP
    /// rate limit bounds.
    fn take_all(&mut self, old: &mut Sockets, spare: &mut Vec<SocketHandle>) {
        debug_assert!(self.used == 0 && self.slots >= old.slots);
        spare.clear();
        let mut moved = 0;
        let mut from = old.set.iter_mut().peekable();
        for (handle, slot) in self.set.iter_mut() {
            match from.next_if(|(h, _)| *h == handle) {
                Some((_, s)) => {
                    core::mem::swap(slot, s);
                    moved += 1;
                }
                None => spare.push(handle),
            }
        }
        // Never a socket left behind or under a wrong handle silently: a connection would lose its data or
        // get another one's.
        assert!(from.next().is_none() && moved == old.used, "{moved} of {} sockets moved", old.used);
        for h in spare.drain(..) {
            drop(self.set.remove(h));
        }
        self.used = old.used;
        self.kept = old.kept;
        old.kept = 0;
    }
}

impl core::ops::Deref for Sockets {
    type Target = SocketSet<'static>;
    fn deref(&self) -> &SocketSet<'static> {
        &self.set
    }
}

/// The parts of a card that have one place per socket.
struct Places<'a> {
    sockets: &'a mut Sockets,
    listen: &'a mut Vec<ListenSock>,
    orphans: &'a mut Vec<Orphan>,
}

struct Fresh {
    sockets: Sockets,
    /// Slots of `sockets` that hold a filler, from the first on.
    filled: usize,
    listen: Vec<ListenSock>,
    orphans: Vec<Orphan>,
    /// Room for the handles of the fillers no socket took the place of.
    spare: Vec<SocketHandle>,
}

impl Fresh {
    /// A table of `want` slots with fillers in the first `filled`, out of lists that have the room: `table`,
    /// `listen` and `orphans` for `want` entries, `spare` for `filled`. No lock held.
    fn over(table: Vec<SocketStorage<'static>>, listen: Vec<ListenSock>, orphans: Vec<Orphan>, spare: Vec<SocketHandle>, want: usize, filled: usize) -> Self {
        debug_assert!(listen.capacity() >= want && orphans.capacity() >= want && spare.capacity() >= filled);
        let mut sockets = Sockets::over(table, want);
        sockets.fill(filled);
        Fresh { sockets, filled, listen, orphans, spare }
    }
}

/// A larger table in the making. Declared before the card's guard: the old table is freed after the unlock.
pub(crate) struct Room {
    fresh: Option<Fresh>,
    want: usize,
    /// Slots the table had at the last look: the larger one needs a filler in each.
    had: usize,
}

impl Room {
    pub(crate) const fn new() -> Self {
        Room { fresh: None, want: 0, had: 0 }
    }

    /// Under the card's lock: whether `n` slots are free now (the larger table is swapped in when it is large
    /// enough). False: drop the lock, `grow`, take the lock and ask again.
    fn fits(&mut self, p: Places<'_>, n: usize) -> bool {
        if p.sockets.free() >= n {
            return true;
        }
        let need = p.sockets.used() + n;
        // A table another thread made larger meanwhile has slots this one has no filler for: made again.
        if let Some(f) = self.fresh.as_mut().filter(|f| f.sockets.slots() >= need && f.filled >= p.sockets.slots()) {
            f.sockets.take_all(p.sockets, &mut f.spare);
            core::mem::swap(p.sockets, &mut f.sockets);
            // Plain moves: both have room for every slot of the larger table.
            f.listen.append(p.listen);
            core::mem::swap(p.listen, &mut f.listen);
            f.orphans.append(p.orphans);
            core::mem::swap(p.orphans, &mut f.orphans);
            return true;
        }
        // Doubling keeps a table that fills one by one from coming back here every time.
        self.want = need.max(p.sockets.slots() * 2);
        self.had = p.sockets.slots();
        false
    }

    /// With no spin lock held: makes what the last `fits` missed. `Err`: no memory (`Stack::buffer`'s rule: asked
    /// for, never assumed; the tests' injected refusals count here too).
    fn grow(&mut self, stack: &Stack) -> Result<(), NetError> {
        // The stale one goes first, so both are never held at once.
        self.fresh = None;
        let (listen, orphans, spare) = (stack.room(self.want)?, stack.room(self.want)?, stack.room(self.had)?);
        self.fresh = Some(Fresh::over(stack.room(self.want)?, listen, orphans, spare, self.want, self.had));
        Ok(())
    }
}

impl Inner {
    fn places(&mut self) -> Places<'_> {
        Places { sockets: &mut self.sockets, listen: &mut self.listen, orphans: &mut self.orphans }
    }

    /// Free slots other sockets leave alone: what the kept pool (the MCP reserve, made while memory was
    /// there) may still put on this card once `adding` more of its sockets are in the table. So a table that
    /// cannot grow never keeps MCP's listening sockets out, whatever programs opened (V2-PLAN §6).
    fn held_back(&self, stack: &Stack, adding: usize) -> usize {
        stack.kept_per_card().saturating_sub(self.sockets.kept() + adding)
    }

    /// Whether `kept` sockets of the kept pool and `other` sockets can be added now.
    pub(crate) fn admits(&self, stack: &Stack, kept: usize, other: usize) -> bool {
        let free = self.sockets.free();
        if other == 0 {
            return free >= kept;
        }
        free >= kept + other + self.held_back(stack, kept)
    }
}

impl Iface {
    /// The card's lock with free slots in the socket table for `kept` sockets of the kept pool and `other`
    /// sockets, and whether they are there (false: no memory for a larger table; `Inner::admits` then says
    /// what still fits). The larger table is made with the lock dropped: under it the heap can neither
    /// reclaim nor fall back to single frames. `room` is declared before the guard is bound, so the old table
    /// is freed after the unlock.
    ///
    /// The lock is dropped only before the caller has looked at anything, so no check of the caller's is
    /// stale; the slots cannot go between the last look here and the caller's adds, which happen under the
    /// same hold. `background` (the poll round): after a refusal the heap is asked again only after
    /// `Stack::bg_alloc_open`'s pause, as for the sockets themselves.
    pub(crate) fn lock_room(&self, stack: &Stack, room: &mut Room, kept: usize, other: usize, background: bool) -> (SpinLockGuard<'_, Inner>, bool) {
        loop {
            let mut g = self.inner.lock();
            let need = kept + other + g.held_back(stack, kept);
            if room.fits(g.places(), need) {
                return (g, true);
            }
            drop(g);
            let made = (!background || stack.bg_alloc_open()) && {
                let made = room.grow(stack).is_ok();
                if background {
                    stack.bg_alloc_done(made);
                }
                made
            };
            if !made {
                let g = self.inner.lock();
                let ok = g.admits(stack, kept, other);
                return (g, ok);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::socket::udp;
    use smoltcp::wire::IpListenEndpoint;

    /// A UDP socket without buffers, bound to `port`: its port tells afterwards which socket a handle names.
    fn marked(port: u16) -> udp::Socket<'static> {
        let buf = || udp::PacketBuffer::new(Vec::new(), Vec::new());
        let mut s = udp::Socket::new(buf(), buf());
        s.bind(IpListenEndpoint { addr: None, port }).unwrap();
        s
    }

    fn table(slots: usize) -> Sockets {
        Sockets::over(Vec::with_capacity(slots), slots)
    }

    /// What `Room::grow` makes, without a stack.
    fn fresh(want: usize, had: usize) -> Fresh {
        Fresh::over(Vec::with_capacity(want), Vec::with_capacity(want), Vec::with_capacity(want), Vec::with_capacity(had), want, had)
    }

    fn port(t: &Sockets, h: SocketHandle) -> u16 {
        t.get::<udp::Socket>(h).endpoint().port
    }

    struct Card {
        sockets: Sockets,
        listen: Vec<ListenSock>,
        orphans: Vec<Orphan>,
    }

    impl Card {
        fn new(slots: usize) -> Card {
            Card { sockets: table(slots), listen: Vec::with_capacity(slots), orphans: Vec::with_capacity(slots) }
        }
        fn places(&mut self) -> Places<'_> {
            Places { sockets: &mut self.sockets, listen: &mut self.listen, orphans: &mut self.orphans }
        }
        /// `Room`'s protocol without a lock or a stack.
        fn room(&mut self, n: usize) {
            let mut room = Room::new();
            while !room.fits(self.places(), n) {
                room.fresh = Some(fresh(room.want, room.had));
            }
        }
    }

    #[test]
    fn a_full_table_moves_with_every_handle() {
        let mut c = Card::new(4);
        let hs: Vec<(SocketHandle, u16)> = (0..4).map(|i| (c.sockets.add(marked(1000 + i), i == 1), 1000 + i)).collect();
        assert_eq!((c.sockets.free(), c.sockets.kept()), (0, 1));
        c.room(1);
        assert_eq!((c.sockets.slots(), c.sockets.used(), c.sockets.kept()), (8, 4, 1));
        for (h, p) in &hs {
            assert_eq!(port(&c.sockets, *h), *p);
        }
        assert_eq!(c.sockets.iter().count(), 4, "no filler stayed behind");
        assert!(c.listen.capacity() >= 8 && c.orphans.capacity() >= 8, "the lists have a place per slot");
        // The next sockets take the new slots without another table.
        for i in 0..4 {
            let h = c.sockets.add(marked(2000 + i), false);
            assert_eq!(port(&c.sockets, h), 2000 + i);
        }
        assert_eq!(c.sockets.free(), 0);
        for (h, p) in &hs {
            assert_eq!(port(&c.sockets, *h), *p);
        }
    }

    /// Room for several sockets is asked for while some slots are empty in the middle: the sockets around the
    /// holes keep their handles, and the holes are empty slots again afterwards.
    #[test]
    fn a_table_with_holes_moves_with_every_handle() {
        let mut c = Card::new(8);
        let mut hs: Vec<(SocketHandle, u16)> = (0..8).map(|i| (c.sockets.add(marked(1000 + i), false), 1000 + i)).collect();
        for i in [6usize, 3, 0] {
            let (h, _) = hs.remove(i);
            c.sockets.remove(h, false);
        }
        assert_eq!(c.sockets.free(), 3);
        c.room(5);
        assert_eq!((c.sockets.slots(), c.sockets.used()), (16, 5));
        assert_eq!(c.sockets.iter().count(), 5, "the fillers of the three holes are gone");
        for (h, p) in &hs {
            assert_eq!(port(&c.sockets, *h), *p);
        }
        // smoltcp fills the lowest hole first: the freed handles come back in order.
        let again: Vec<SocketHandle> = (0..3).map(|i| c.sockets.add(marked(3000 + i), false)).collect();
        assert!(again.windows(2).all(|w| w[0] < w[1]));
        assert!(again[2] < hs.last().unwrap().0, "holes are used before the new slots");
        for (h, p) in &hs {
            assert_eq!(port(&c.sockets, *h), *p);
        }
    }

    /// The table changed between the unlock and the relock: a spare made for the old state is used only when it
    /// still holds everything, else a larger one is asked for.
    #[test]
    fn a_spare_that_became_too_small_is_not_used() {
        let mut c = Card::new(2);
        c.sockets.add(marked(1), false);
        c.sockets.add(marked(2), false);
        let mut room = Room::new();
        assert!(!room.fits(c.places(), 1));
        assert_eq!((room.want, room.had), (4, 2));
        room.fresh = Some(fresh(4, 2));
        // Another thread got there first and filled its own larger table meanwhile.
        c.room(6);
        for i in 0..6 {
            c.sockets.add(marked(10 + i), false);
        }
        assert_eq!((c.sockets.slots(), c.sockets.free()), (8, 0));
        assert!(!room.fits(c.places(), 1), "4 slots do not hold 8 sockets");
        assert_eq!((room.want, room.had), (16, 8));
        // Large enough, but with fillers for a table of 2 slots only (made before the other thread's growth):
        // the sockets of slots 2 to 7 would have no place to change into.
        room.fresh = Some(fresh(16, 2));
        assert!(!room.fits(c.places(), 1), "fillers for 2 slots do not take 8 sockets");
        room.fresh = Some(fresh(16, 8));
        assert!(room.fits(c.places(), 1));
        assert_eq!((c.sockets.slots(), c.sockets.used()), (16, 8));
        assert_eq!(c.sockets.iter().count(), 8, "no filler stayed behind");
        // Room that came by itself (a socket went): the spare stays unused.
        let mut room = Room::new();
        assert!(room.fits(c.places(), 8));
        assert!(room.fresh.is_none());
    }

    #[test]
    fn many_sockets_keep_their_handles_through_every_doubling() {
        let mut c = Card::new(FIRST_SLOTS);
        let mut live: Vec<(SocketHandle, u16)> = Vec::new();
        let mut next = 1u16;
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for step in 0..3000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // More adds than removes, in bursts of up to 5 that need their room at once.
            if x % 3 == 0 && !live.is_empty() {
                let (h, _) = live.swap_remove((x >> 8) as usize % live.len());
                c.sockets.remove(h, false);
            } else {
                let n = 1 + (x >> 16) as usize % 5;
                c.room(n);
                for _ in 0..n {
                    live.push((c.sockets.add(marked(next), false), next));
                    next += 1;
                }
            }
            if step % 97 == 0 {
                for (h, p) in &live {
                    assert_eq!(port(&c.sockets, *h), *p, "step {step}");
                }
            }
        }
        assert_eq!(c.sockets.used(), live.len());
        assert_eq!(c.sockets.iter().count(), live.len());
        assert!(c.sockets.slots() >= 1024, "{} slots", c.sockets.slots());
        for (h, p) in &live {
            assert_eq!(port(&c.sockets, *h), *p);
        }
    }
}
