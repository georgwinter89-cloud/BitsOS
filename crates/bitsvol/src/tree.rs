//! Copy-on-write radix trees of pages: a volume's mapping (virtual chunk -> (chunk, write time)) and the volume
//! table (id -> record page). Every page on a changed path is shadowed once per transaction; a shared page
//! (count > 1) hands one reference to each child when it is copied (Linux dm-thin's lazy sharing).

use crate::error::{Result, VolError};
use crate::layout::{self, DEPTH_BOUND, INNER_FANOUT, KIND_INNER, KIND_LEAF, LEAF_FANOUT, Tree, capacity, child_base, slot};
use crate::pool::Pool;
use alloc::vec::Vec;

pub(crate) type Entry = (u64, u64);

fn kind_of(level: u32) -> u8 {
    if level == 0 { KIND_LEAF } else { KIND_INNER }
}

pub(crate) fn check(b: &[u8], level: u32, tree: Tree) -> Result<()> {
    if layout::kind(b) != (kind_of(level), level as u8, tree as u8) {
        return Err(VolError::Corrupt("page kind or level"));
    }
    Ok(())
}

impl Pool {
    fn checked_page(&mut self, p: u64, level: u32, tree: Tree) -> Result<&[u8]> {
        let b = self.page(p)?;
        check(b, level, tree)?;
        Ok(b)
    }

    pub(crate) fn tree_get(&mut self, root: u64, depth: u32, tree: Tree, key: u64) -> Result<Entry> {
        if root == 0 || key >= capacity(depth) {
            return Ok((0, 0));
        }
        let mut p = root;
        for level in (0..depth).rev() {
            let b = self.checked_page(p, level, tree)?;
            let s = slot(key, level);
            if level == 0 {
                return Ok(layout::leaf_get(b, s));
            }
            p = layout::inner_get(b, s);
            if p == 0 {
                return Ok((0, 0));
            }
        }
        Ok((0, 0))
    }

    /// A page of this transaction with the content of `p`, holding the caller's reference.
    fn shadow(&mut self, p: u64, level: u32, tree: Tree) -> Result<u64> {
        let rc = self.page_rc(p)?;
        if rc == 1 && self.is_dirty(p) {
            return Ok(p);
        }
        let mut copy = Vec::new();
        copy.try_reserve_exact(layout::PAGE).map_err(|_| VolError::NoMemory)?;
        copy.extend_from_slice(self.checked_page(p, level, tree)?);
        let q = self.alloc_page(kind_of(level), level as u8, tree)?;
        if rc > 1 {
            self.share_children(&copy, level, tree)?;
        }
        self.page_mut(q)?.copy_from_slice(&copy);
        self.page_dec(p)?;
        Ok(q)
    }

    fn share_children(&mut self, b: &[u8], level: u32, tree: Tree) -> Result<()> {
        if level > 0 {
            for i in 0..INNER_FANOUT {
                let c = layout::inner_get(b, i);
                if c != 0 {
                    self.page_inc(c)?;
                }
            }
            return Ok(());
        }
        for i in 0..LEAF_FANOUT {
            let (v, _) = layout::leaf_get(b, i);
            if v != 0 {
                match tree {
                    Tree::Map => self.chunk_inc(v)?,
                    Tree::Table => self.page_inc(v)?,
                }
            }
        }
        Ok(())
    }

    /// Sets `key` to `v` in the tree `t` = (root, depth) and returns the old entry. Clearing an entry frees the
    /// pages left empty. `t` is kept current at every step, also when this fails half way (a grown or shadowed
    /// root is then already the tree's). The caller owns the references the entries stand for.
    pub(crate) fn tree_set(&mut self, t: &mut (u64, u32), tree: Tree, key: u64, v: Entry) -> Result<Entry> {
        let clear = v == (0, 0);
        if clear && self.tree_get(t.0, t.1, tree, key)? == (0, 0) {
            return Ok((0, 0));
        }
        while key >= capacity(t.1) {
            if t.1 >= DEPTH_BOUND {
                return Err(VolError::InvalidArgument);
            }
            if t.0 != 0 {
                let n = self.alloc_page(KIND_INNER, t.1 as u8, tree)?;
                layout::inner_set(self.page_mut(n)?, 0, t.0);
                t.0 = n;
            }
            t.1 += 1;
        }
        let depth = t.1;
        t.0 = if t.0 == 0 { self.alloc_page(kind_of(depth - 1), (depth - 1) as u8, tree)? } else { self.shadow(t.0, depth - 1, tree)? };
        let mut path: Vec<(u64, u64)> = Vec::new();
        path.try_reserve(depth as usize).map_err(|_| VolError::NoMemory)?;
        let mut p = t.0;
        for level in (1..depth).rev() {
            let s = slot(key, level);
            let c = layout::inner_get(self.page(p)?, s);
            let c2 = if c == 0 { self.alloc_page(kind_of(level - 1), (level - 1) as u8, tree)? } else { self.shadow(c, level - 1, tree)? };
            if c2 != c {
                layout::inner_set(self.page_mut(p)?, s, c2);
            }
            path.push((p, s));
            p = c2;
        }
        let s = slot(key, 0);
        let old = layout::leaf_get(self.page(p)?, s);
        layout::leaf_set(self.page_mut(p)?, s, v);
        if clear {
            let mut child = p;
            while layout::entries_zero(self.page(child)?) {
                self.page_dec(child)?;
                match path.pop() {
                    Some((parent, s)) => {
                        layout::inner_set(self.page_mut(parent)?, s, 0);
                        child = parent;
                    }
                    None => {
                        t.0 = 0;
                        break;
                    }
                }
            }
        }
        Ok(old)
    }

    /// Drops one reference to the subtree at `p`; pages and chunks left without any are freed.
    pub(crate) fn drop_tree(&mut self, p: u64, level: u32, tree: Tree) -> Result<()> {
        if self.page_rc(p)? > 1 {
            self.page_dec(p)?;
            return Ok(());
        }
        let mut kids: Vec<u64> = Vec::new();
        {
            let n = if level > 0 { INNER_FANOUT } else { LEAF_FANOUT };
            kids.try_reserve(n as usize).map_err(|_| VolError::NoMemory)?;
            let b = self.checked_page(p, level, tree)?;
            for i in 0..n {
                let c = if level > 0 { layout::inner_get(b, i) } else { layout::leaf_get(b, i).0 };
                if c != 0 {
                    kids.push(c);
                }
            }
        }
        self.page_dec(p)?;
        for c in kids {
            match (level, tree) {
                (0, Tree::Map) => self.chunk_dec(c)?,
                (0, Tree::Table) => {
                    self.page_dec(c)?;
                }
                _ => self.drop_tree(c, level - 1, tree)?,
            }
        }
        Ok(())
    }

    /// Up to `limit` mapped entries with key >= `from`, in key order.
    pub(crate) fn tree_walk(&mut self, root: u64, depth: u32, tree: Tree, from: u64, limit: usize, out: &mut Vec<(u64, Entry)>) -> Result<()> {
        if root == 0 || depth == 0 || from >= capacity(depth) {
            return Ok(());
        }
        self.walk_page(root, depth - 1, 0, tree, from, limit, out)
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_page(&mut self, p: u64, level: u32, base: u64, tree: Tree, from: u64, limit: usize, out: &mut Vec<(u64, Entry)>) -> Result<()> {
        let mut ents: Vec<(u64, Entry)> = Vec::new();
        {
            let b = self.checked_page(p, level, tree)?;
            let n = if level > 0 { INNER_FANOUT } else { LEAF_FANOUT };
            for i in 0..n {
                let e = if level > 0 { (layout::inner_get(b, i), 0) } else { layout::leaf_get(b, i) };
                let end = child_base(base, level, i + 1);
                if e.0 != 0 && end > from {
                    ents.try_reserve(1).map_err(|_| VolError::NoMemory)?;
                    ents.push((child_base(base, level, i), e));
                }
            }
        }
        for (k, e) in ents {
            if out.len() >= limit {
                return Ok(());
            }
            if level == 0 {
                out.try_reserve(1).map_err(|_| VolError::NoMemory)?;
                out.push((k, e));
            } else {
                self.walk_page(e.0, level - 1, k, tree, from, limit, out)?;
            }
        }
        Ok(())
    }

    /// Chunks only this tree holds: reached through pages of count 1 and with count 1 themselves.
    pub(crate) fn exclusive(&mut self, p: u64, level: u32) -> Result<u64> {
        if self.page_rc(p)? > 1 {
            return Ok(0);
        }
        let mut kids: Vec<u64> = Vec::new();
        {
            let n = if level > 0 { INNER_FANOUT } else { LEAF_FANOUT };
            let b = self.checked_page(p, level, Tree::Map)?;
            for i in 0..n {
                let c = if level > 0 { layout::inner_get(b, i) } else { layout::leaf_get(b, i).0 };
                if c != 0 {
                    kids.try_reserve(1).map_err(|_| VolError::NoMemory)?;
                    kids.push(c);
                }
            }
        }
        if level == 0 {
            return Ok(kids.iter().filter(|&&c| self.crefs.get(c) == 1).count() as u64);
        }
        let mut n = 0;
        for c in kids {
            n += self.exclusive(c, level - 1)?;
        }
        Ok(n)
    }
    /// `exclusive` for the keys from `from` on in the subtree at `p` (first key `base`), reading at most `*left`
    /// pages; `Some(key)`: stopped there, nothing from `key` on counted yet.
    pub(crate) fn exclusive_from(&mut self, p: u64, level: u32, base: u64, from: u64, left: &mut u64, n: &mut u64) -> Result<Option<u64>> {
        if self.page_rc(p)? > 1 {
            return Ok(None);
        }
        if *left == 0 {
            return Ok(Some(base.max(from)));
        }
        *left -= 1;
        let mut kids: Vec<(u64, u64)> = Vec::new();
        {
            let cnt = if level > 0 { INNER_FANOUT } else { LEAF_FANOUT };
            let b = self.checked_page(p, level, Tree::Map)?;
            for i in 0..cnt {
                let c = if level > 0 { layout::inner_get(b, i) } else { layout::leaf_get(b, i).0 };
                let first = child_base(base, level, i);
                let end = if level > 0 { child_base(base, level, i + 1) } else { first.saturating_add(1) };
                if c != 0 && end > from {
                    kids.try_reserve(1).map_err(|_| VolError::NoMemory)?;
                    kids.push((first, c));
                }
            }
        }
        if level == 0 {
            *n += kids.iter().filter(|&&(_, c)| self.crefs.get(c) == 1).count() as u64;
            return Ok(None);
        }
        for (first, c) in kids {
            if let Some(k) = self.exclusive_from(c, level - 1, first, from, left, n)? {
                return Ok(Some(k));
            }
        }
        Ok(None)
    }
}
