//! L7-RESTE ktest of /proc/<pid>/pagemap and smaps' "gu" as a program sees them: `abi-pagemap` against CachyOS's
//! transcripts (the cases `cargo xtask abi-diff abi-pagemap` runs). `sticky` is held to V2's own answer until
//! crates/mm has VM_MAYBE_GUARD (open item in plan/notes/desk-offen.md, group G3): it turns red when that changes.

use crate::ab7_ktests::program_case;
use ktest::{TestResult, ktest};

const EXPECTED: &str = include_str!("../../../../user/abi-tests/expected/l7reste-abi-pagemap.txt");
const EXEC: &str = include_str!("../../../../user/abi-tests/expected/l7reste-abi-pagemap-exec.txt");
const STICKY_V2: &str = include_str!("../../../../user/abi-tests/expected/l7reste-abi-pagemap-sticky-v2.txt");

fn programs() -> TestResult {
    program_case("/bin/abi-pagemap", &["abi-pagemap"], EXPECTED)?;
    program_case("/bin/abi-pagemap", &["abi-pagemap", "exec"], EXEC)?;
    program_case("/bin/abi-pagemap", &["abi-pagemap", "sticky"], STICKY_V2)
}
ktest!("fs.l7reste.pagemap_programs", Safe, programs);
