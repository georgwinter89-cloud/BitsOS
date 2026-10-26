//! Extent trees (linux-6.6/fs/ext4/ext4_extents.h, extents.c): lookup, and one edit primitive that maps or
//! unmaps a range of file blocks, splitting extents, splitting full nodes (the root in the inode grows a
//! level), removing empty nodes, keeping every index key equal to its child's first block (e2fsck checks
//! that) and pulling a small tree back into the inode. Truncate, writes into holes and the conversion of
//! unwritten extents are all that edit.
//!
//! Hostile trees: magic, counts, depth (at most 5, `EXT4_MAX_EXTENT_DEPTH`), key order and block checksums
//! are checked on every node read; a bad node is EUCLEAN (EBADMSG for its checksum), never a panic or a loop.

use crate::crc::crc32c;
use crate::inode::DiskInode;
use crate::le::*;
use crate::state::State;
use crate::tx::Tx;
use alloc::vec;
use alloc::vec::Vec;
use crate::errno::*;

const MAGIC: u16 = 0xF30A;
const MAX_DEPTH: u16 = 5;
pub const INIT_MAX: u32 = 32768;
pub const UNWRITTEN_MAX: u32 = 32767;

/// A mapping of `len` file blocks from `lblk` to disk blocks from `pblk`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ext {
    pub lblk: u32,
    pub len: u32,
    pub pblk: u64,
    pub unwritten: bool,
}

impl Ext {
    fn end(&self) -> u64 {
        self.lblk as u64 + self.len as u64
    }
}

#[derive(Clone, Copy, Debug)]
enum Ent {
    Leaf(Ext),
    Idx { lblk: u32, child: u64 },
}

impl Ent {
    fn lblk(&self) -> u32 {
        match self {
            Ent::Leaf(e) => e.lblk,
            Ent::Idx { lblk, .. } => *lblk,
        }
    }
}

#[derive(Clone)]
struct Node {
    /// None: the root in the inode's i_block.
    blk: Option<u64>,
    depth: u16,
    max: usize,
    ents: Vec<Ent>,
}

/// An empty root (depth 0, room for 4 extents).
pub fn init_root(ib: &mut [u8]) {
    ib.fill(0);
    p16(ib, 0, MAGIC);
    p16(ib, 4, 4);
}

fn cap_block(bs: usize) -> usize {
    (bs - 12) / 12
}

fn parse(b: &[u8], cap: usize, blk: Option<u64>) -> Result<Node, i64> {
    let (n, max, depth) = (g16(b, 2) as usize, g16(b, 4) as usize, g16(b, 6));
    if g16(b, 0) != MAGIC || n > max || max > cap || max == 0 || depth > MAX_DEPTH {
        klog::event!(Warn, "ext4", "extent.bad_header", block = blk.unwrap_or(0), entries = n, max = max, depth = depth);
        return Err(EUCLEAN);
    }
    let mut ents = Vec::with_capacity(n);
    let mut last_end = 0u64;
    for i in 0..n {
        let o = 12 + i * 12;
        let lblk = g32(b, o);
        let e = if depth == 0 {
            let raw = g16(b, o + 4) as u32;
            let (len, unwritten) = if raw > INIT_MAX { (raw - INIT_MAX, true) } else { (raw, false) };
            let pblk = g32(b, o + 8) as u64 | (g16(b, o + 6) as u64) << 32;
            Ent::Leaf(Ext { lblk, len, pblk, unwritten })
        } else {
            Ent::Idx { lblk, child: g32(b, o + 4) as u64 | (g16(b, o + 8) as u64) << 32 }
        };
        let ok = match e {
            Ent::Leaf(x) => x.len > 0 && (lblk as u64) >= last_end,
            Ent::Idx { .. } => i == 0 || lblk as u64 > last_end,
        };
        if !ok {
            klog::event!(Warn, "ext4", "extent.bad_order", block = blk.unwrap_or(0), index = i);
            return Err(EUCLEAN);
        }
        last_end = match e {
            Ent::Leaf(x) => x.end(),
            Ent::Idx { lblk, .. } => lblk as u64,
        };
        ents.push(e);
    }
    Ok(Node { blk, depth, max, ents })
}

fn render(n: &Node, out: &mut [u8]) {
    let len = out.len();
    out[..len.min(12 + n.max * 12)].fill(0);
    p16(out, 0, MAGIC);
    p16(out, 2, n.ents.len() as u16);
    p16(out, 4, n.max as u16);
    p16(out, 6, n.depth);
    for (i, e) in n.ents.iter().enumerate() {
        let o = 12 + i * 12;
        match e {
            Ent::Leaf(x) => {
                p32(out, o, x.lblk);
                p16(out, o + 4, if x.unwritten { x.len + INIT_MAX } else { x.len } as u16);
                p16(out, o + 6, (x.pblk >> 32) as u16);
                p32(out, o + 8, x.pblk as u32);
            }
            Ent::Idx { lblk, child } => {
                p32(out, o, *lblk);
                p32(out, o + 4, *child as u32);
                p16(out, o + 8, (*child >> 32) as u16);
            }
        }
    }
}

/// Whether the inode maps its blocks with extents (else: inline data, a fast symlink or ext2/3 block maps).
pub fn uses_extents(di: &DiskInode) -> bool {
    di.flags() & crate::inode::FL_EXTENTS != 0
}

/// What lies at a file block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    Ext(Ext),
    /// A hole up to the next mapped block (None: to the end).
    Hole(Option<u32>),
}

fn load(st: &State, tx: Option<&Tx>, di: &DiskInode, blk: u64) -> Result<Node, i64> {
    let b = st.read(tx, blk)?;
    let n = parse(&b, cap_block(st.bs()), Some(blk))?;
    if let Some(seed) = di.csum_seed(&st.geo) {
        let t = 12 + n.max * 12;
        if t + 4 <= b.len() && crc32c(seed, &b[..t]) != g32(&b, t) {
            klog::event!(Warn, "ext4", "extent.badcrc", ino = di.ino, block = blk);
            return Err(EBADMSG);
        }
    }
    Ok(n)
}

fn root(di: &DiskInode) -> Result<Node, i64> {
    parse(di.i_block(), 4, None)
}

fn child_index(n: &Node, l: u32) -> usize {
    n.ents.iter().rposition(|e| e.lblk() <= l).unwrap_or(0)
}

/// The extent covering file block `l`, or the hole there.
pub fn lookup(st: &State, tx: Option<&Tx>, di: &DiskInode, l: u32) -> Result<Found, i64> {
    let mut n = root(di)?;
    let mut next: Option<u32> = None;
    while n.depth > 0 {
        if n.ents.is_empty() {
            return Err(EUCLEAN);
        }
        let i = child_index(&n, l);
        if let Some(e) = n.ents.get(i + 1) {
            next = Some(e.lblk());
        }
        let Ent::Idx { child, .. } = n.ents[i] else { return Err(EUCLEAN) };
        let c = load(st, tx, di, child)?;
        if c.depth + 1 != n.depth {
            return Err(EUCLEAN);
        }
        n = c;
    }
    for e in &n.ents {
        let Ent::Leaf(x) = e else { return Err(EUCLEAN) };
        if l >= x.lblk && (l as u64) < x.end() {
            return Ok(Found::Ext(*x));
        }
        if x.lblk > l {
            return Ok(Found::Hole(Some(x.lblk)));
        }
    }
    Ok(Found::Hole(next))
}

/// Every extent in order, and the number of tree blocks (for checks and `st_blocks`).
pub fn all(st: &State, tx: Option<&Tx>, di: &DiskInode) -> Result<(Vec<Ext>, u64), i64> {
    let mut out = Vec::new();
    let mut blocks = 0;
    let mut stack = vec![root(di)?];
    while let Some(n) = stack.pop() {
        for e in n.ents.iter().rev() {
            match *e {
                Ent::Leaf(x) => out.push(x),
                Ent::Idx { child, .. } => {
                    let c = load(st, tx, di, child)?;
                    if c.depth + 1 != n.depth {
                        return Err(EUCLEAN);
                    }
                    blocks += 1;
                    stack.push(c);
                }
            }
        }
    }
    out.sort_by_key(|e| e.lblk);
    Ok((out, blocks))
}

/// What an edit maps a range to.
#[derive(Clone, Copy)]
pub enum Target {
    Map { pblk: u64, unwritten: bool },
    Unmap,
}

/// One edit of one inode's tree: blocks it took off the mapping (`displaced`) and tree blocks gained (+) or
/// given back (-) for `i_blocks`; tree blocks freed are in `freed`.
pub struct Edit<'a> {
    pub st: &'a mut State,
    pub tx: &'a mut Tx,
    pub di: &'a mut DiskInode,
    pub displaced: Vec<(u64, u64)>,
    pub freed: Vec<(u64, u64)>,
    pub tree_blocks: i64,
    pub goal: u64,
}

impl<'a> Edit<'a> {
    pub fn new(st: &'a mut State, tx: &'a mut Tx, di: &'a mut DiskInode, goal: u64) -> Edit<'a> {
        Edit { st, tx, di, displaced: Vec::new(), freed: Vec::new(), tree_blocks: 0, goal }
    }

    fn path(&self, l: u32) -> Result<(Vec<Node>, Vec<usize>), i64> {
        let mut path = vec![root(self.di)?];
        let mut idx = Vec::new();
        loop {
            let n = path.last().ok_or(EUCLEAN)?;
            if n.depth == 0 {
                break;
            }
            if n.ents.is_empty() {
                return Err(EUCLEAN);
            }
            let i = child_index(n, l);
            let Ent::Idx { child, .. } = n.ents[i] else { return Err(EUCLEAN) };
            let c = load(self.st, Some(self.tx), self.di, child)?;
            if c.depth + 1 != n.depth {
                return Err(EUCLEAN);
            }
            idx.push(i);
            path.push(c);
        }
        Ok((path, idx))
    }

    fn store(&mut self, n: &Node) -> Result<(), i64> {
        match n.blk {
            None => render(n, self.di.i_block_mut()),
            Some(b) => {
                let mut buf = vec![0u8; self.st.bs()];
                render(n, &mut buf);
                if let Some(seed) = self.di.csum_seed(&self.st.geo) {
                    let t = 12 + n.max * 12;
                    let c = crc32c(seed, &buf[..t]);
                    p32(&mut buf, t, c);
                }
                self.tx.put(b, buf);
            }
        }
        Ok(())
    }

    fn new_block(&mut self) -> Result<u64, i64> {
        let (b, _) = self.st.alloc_blocks(self.tx, self.goal, 1, 0)?;
        self.tree_blocks += 1;
        self.goal = b + 1;
        Ok(b)
    }

    /// Maps (or unmaps) file blocks [l, l + n). A `Map` range must be at most one extent long.
    pub fn set(&mut self, l: u32, n: u64, t: Target) -> Result<(), i64> {
        let end = (l as u64 + n).min(1 << 32);
        let start = l as u64;
        let mut at = start;
        while at < end {
            let (mut path, idx) = self.path(at as u32)?;
            let mut le = 1u64 << 32;
            for k in (0..idx.len()).rev() {
                if let Some(e) = path[k].ents.get(idx[k] + 1) {
                    le = e.lblk() as u64;
                    break;
                }
            }
            let pe = end.min(le);
            if pe <= at {
                return Err(EUCLEAN);
            }
            let leaf = path.last_mut().ok_or(EUCLEAN)?;
            let at_end = leaf.ents.last().is_none_or(|e| e.lblk() as u64 <= at);
            let mut out: Vec<Ext> = Vec::with_capacity(leaf.ents.len() + 3);
            for e in &leaf.ents {
                let Ent::Leaf(x) = *e else { return Err(EUCLEAN) };
                let (es, ee) = (x.lblk as u64, x.end());
                if ee <= at || es >= pe {
                    out.push(x);
                    continue;
                }
                if es < at {
                    out.push(Ext { len: (at - es) as u32, ..x });
                }
                let (os, oe) = (es.max(at), ee.min(pe));
                self.displaced.push((x.pblk + (os - es), oe - os));
                if ee > pe {
                    out.push(Ext { lblk: pe as u32, len: (ee - pe) as u32, pblk: x.pblk + (pe - es), unwritten: x.unwritten });
                }
            }
            if let Target::Map { pblk, unwritten } = t {
                let max = if unwritten { UNWRITTEN_MAX } else { INIT_MAX } as u64;
                let mut s = at;
                while s < pe {
                    let len = (pe - s).min(max);
                    out.push(Ext { lblk: s as u32, len: len as u32, pblk: pblk + (s - start), unwritten });
                    s += len;
                }
            }
            out.sort_by_key(|e| e.lblk);
            let mut merged: Vec<Ext> = Vec::with_capacity(out.len());
            for x in out {
                if let Some(p) = merged.last_mut() {
                    let max = if p.unwritten { UNWRITTEN_MAX } else { INIT_MAX };
                    if p.unwritten == x.unwritten && p.end() == x.lblk as u64 && p.pblk + p.len as u64 == x.pblk && p.len + x.len <= max {
                        p.len += x.len;
                        continue;
                    }
                }
                merged.push(x);
            }
            leaf.ents = merged.into_iter().map(Ent::Leaf).collect();
            self.fix(path, idx, at_end)?;
            at = pe;
        }
        Ok(())
    }

    /// Writes the edited path back from the leaf up: splits nodes over their capacity, drops empty ones, sets
    /// index keys, and finally pulls a tree that fits back into the inode.
    fn fix(&mut self, mut path: Vec<Node>, idx: Vec<usize>, at_end: bool) -> Result<(), i64> {
        let cap = cap_block(self.st.bs());
        let mut level = path.len() - 1;
        loop {
            if path[level].ents.len() > path[level].max {
                if level == 0 {
                    let nb = self.new_block()?;
                    let r = &mut path[0];
                    let child = Node { blk: Some(nb), depth: r.depth, max: cap, ents: core::mem::take(&mut r.ents) };
                    if child.ents.len() > cap {
                        return Err(EUCLEAN);
                    }
                    let first = child.ents[0].lblk();
                    r.depth += 1;
                    r.ents = vec![Ent::Idx { lblk: first, child: nb }];
                    self.store(&child)?;
                    let r = path[0].clone();
                    self.store(&r)?;
                    return self.shrink();
                }
                let len = path[level].ents.len();
                let sp = if at_end { path[level].max.min(len - 1) } else { len / 2 };
                let nb = self.new_block()?;
                let right_ents = path[level].ents.split_off(sp);
                let right = Node { blk: Some(nb), depth: path[level].depth, max: cap, ents: right_ents };
                self.store(&right)?;
                let left = path[level].clone();
                self.store(&left)?;
                let pi = idx[level - 1];
                let p = &mut path[level - 1];
                if let Some(Ent::Idx { lblk, .. }) = p.ents.get_mut(pi) {
                    *lblk = left.ents[0].lblk();
                }
                p.ents.insert(pi + 1, Ent::Idx { lblk: right.ents[0].lblk(), child: nb });
                level -= 1;
                continue;
            }
            if path[level].ents.is_empty() && level > 0 {
                if let Some(b) = path[level].blk {
                    self.freed.push((b, 1));
                    self.tree_blocks -= 1;
                }
                path[level - 1].ents.remove(idx[level - 1]);
                level -= 1;
                continue;
            }
            let n = path[level].clone();
            if level == 0 {
                let mut r = n;
                if r.ents.is_empty() {
                    r.depth = 0;
                }
                self.store(&r)?;
                return self.shrink();
            }
            self.store(&n)?;
            let first = n.ents[0].lblk();
            if let Some(Ent::Idx { lblk, .. }) = path[level - 1].ents.get_mut(idx[level - 1]) {
                *lblk = first;
            }
            level -= 1;
        }
    }

    /// While the root has one child whose entries fit in the root, the child moves up
    /// (`ext4_ext_try_to_merge_up`).
    fn shrink(&mut self) -> Result<(), i64> {
        loop {
            let r = root(self.di)?;
            if r.depth == 0 || r.ents.len() != 1 {
                return Ok(());
            }
            let Ent::Idx { child, .. } = r.ents[0] else { return Err(EUCLEAN) };
            let c = load(self.st, Some(self.tx), self.di, child)?;
            if c.ents.len() > 4 || c.depth + 1 != r.depth {
                return Ok(());
            }
            let up = Node { blk: None, depth: c.depth, max: 4, ents: c.ents };
            self.store(&up)?;
            self.freed.push((child, 1));
            self.tree_blocks -= 1;
        }
    }
}
