//! ATOMIC-3: a program's write to its terminal makes the screen's bytes (OPOST) and grows the console's capture
//! ring with no spin lock held (`tty.state`, `console.capture`): there the kernel's heap can neither reclaim nor
//! wait (5 to 11 KiB per write, Mini-PC 12.11.2026). The allocator here records the largest block asked for while
//! this thread holds a spin lock (ksync's preempt count).

use mm_core::host_count::largest_under_spin;

#[global_allocator]
static ALLOC: mm_core::host_count::Counting = mm_core::host_count::Counting;

const TWO_PAGES: usize = 8192;

/// A piece of newlines (ONLCR doubles it) and enough writes to grow the capture ring to its MiB: no block of two
/// pages or more under a spin lock, the output as OPOST makes it, the capture ends with what was written.
#[test]
fn terminal_writes_grow_their_buffers_outside_spin_locks() {
    let piece = vec![b'\n'; 4096];
    let (big, out) = largest_under_spin(|| console::tty().write(&piece, false, &|| false, false).unwrap());
    assert!(big < TWO_PAGES, "a block of {big} bytes under tty.state");
    assert_eq!(out.len(), 2 * piece.len(), "ONLCR: each newline is CR LF");
    assert!(out.chunks(2).all(|c| c == b"\r\n"));
    let text: Vec<u8> = (0..64 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
    let (big, ()) = largest_under_spin(|| {
        for _ in 0..24 {
            assert_eq!(console::ConsoleDev::write_on(&text, false, &|| false, false), Ok(text.len()));
        }
    });
    assert!(big < TWO_PAGES, "a block of {big} bytes under a spin lock while capturing");
    let cap = console::captured();
    assert_eq!(cap.len(), 1 << 20, "the ring keeps the last MiB");
    assert!(cap.ends_with(&text));
}
