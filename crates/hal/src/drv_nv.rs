//! Unsafe primitives of the NV driver lane (ring barriers, MMIO polls with timeouts, ...). Owner: NV.
//! Every unsafe block needs a `// SAFETY:` comment; the hal checker reviews each item touching this file.

/// BL-1: the device address of `buf` when a bus-master device may read or write it in place: the whole buffer lies in
/// the kernel's direct map (heap blocks from frames, page-cache frames), where virtual and physical addresses differ by
/// one offset, so it is physically contiguous and its first byte's physical address is its device address (IOMMU off,
/// CPU caches snooped on x86). None for anything else: a virtually contiguous (`vmalloc`/kvmalloc scatter) block, the
/// kernel image, a stack outside the direct map, an empty buffer.
///
/// The address is only valid while the caller holds `buf` borrowed: the NVMe driver (`crates/drivers/nvme/src/io.rs`)
/// returns from a request only once its command completed or the device's DMA was stopped after it was placed
/// (`Controller::stop_gen`), so the device never writes memory that was given back.
pub fn dma_target(buf: &[u8]) -> Option<u64> {
    let len = buf.len() as u64;
    if len == 0 {
        return None;
    }
    let start = buf.as_ptr() as u64;
    let last = start.checked_add(len - 1)?;
    let p0 = crate::dma::virt_to_phys(start)?;
    let p1 = crate::dma::virt_to_phys(last)?;
    (p1.checked_sub(p0)? == len - 1).then_some(p0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_direct_map_is_a_dma_target() {
        assert_eq!(super::dma_target(&[]), None);
        let v = [7u8; 64];
        assert_eq!(super::dma_target(&v), None, "a host address is not in the kernel's direct map");
    }
}
