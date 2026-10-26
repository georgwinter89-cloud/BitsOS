//! Items of a report descriptor (HID 1.11 §6.2.2.2 short items, §6.2.2.3 long items).

/// Item type from bits 2-3 of a short item's prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemType {
    Main,
    Global,
    Local,
    Reserved,
    /// A long item (prefix 0xFE). No long item tags are defined, so they are skipped.
    Long,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub offset: usize,
    pub kind: ItemType,
    pub tag: u8,
    /// Data size in bytes: 0, 1, 2 or 4 for short items.
    pub size: u8,
    /// Data zero-extended.
    pub data: u32,
}

impl Item {
    /// Data sign-extended from its size (Linux `item_sdata`).
    pub fn sdata(&self) -> i32 {
        match self.size {
            1 => self.data as u8 as i8 as i32,
            2 => self.data as u16 as i16 as i32,
            4 => self.data as i32,
            _ => 0,
        }
    }
}

/// Walks the items of a descriptor. Yields `Err(offset)` once for an item that runs past the end.
pub struct Items<'a> {
    desc: &'a [u8],
    pos: usize,
    failed: bool,
}

pub fn items(desc: &[u8]) -> Items<'_> {
    Items {
        desc,
        pos: 0,
        failed: false,
    }
}

impl Iterator for Items<'_> {
    type Item = Result<Item, usize>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.desc.len() {
            return None;
        }
        let at = self.pos;
        let rest = &self.desc[at..];
        let prefix = rest[0];
        if prefix == 0xFE {
            // bDataSize, bLongItemTag, then the data.
            let len = rest.get(1).map(|&n| 3 + n as usize);
            match len {
                Some(len) if len <= rest.len() => {
                    self.pos += len;
                    return Some(Ok(Item {
                        offset: at,
                        kind: ItemType::Long,
                        tag: rest[2],
                        size: 0,
                        data: 0,
                    }));
                }
                _ => {
                    self.failed = true;
                    return Some(Err(at));
                }
            }
        }
        let size = [0usize, 1, 2, 4][(prefix & 3) as usize];
        if 1 + size > rest.len() {
            self.failed = true;
            return Some(Err(at));
        }
        let data = rest[1..1 + size]
            .iter()
            .rev()
            .fold(0u32, |acc, &b| acc << 8 | b as u32);
        let kind = match (prefix >> 2) & 3 {
            0 => ItemType::Main,
            1 => ItemType::Global,
            2 => ItemType::Local,
            _ => ItemType::Reserved,
        };
        self.pos += 1 + size;
        Some(Ok(Item {
            offset: at,
            kind,
            tag: prefix >> 4,
            size: size as u8,
            data,
        }))
    }
}
