//! Host tests over register images: the G-1 snapshot (`dcn314_regs::P1_SNAPSHOT`), saved-5.json (DP audio gates
//! of 04.10., tests/data) and broken copies of both (V1 `dcn-test.mjs` gate list).

extern crate std;

mod cursor;
mod fake;
mod flip;
mod gates;
mod layout;
mod mode;
mod model;
mod snap;
