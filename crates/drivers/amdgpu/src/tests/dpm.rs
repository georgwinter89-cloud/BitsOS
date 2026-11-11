//! GX-9b: clocks, GFXOFF and the tool on the host (the ktests' checks).

use crate::ktests_dpm as k;
use crate::ktests_dpm_edges as e;

#[test]
fn tables_and_units() {
    k::tables().unwrap();
}

#[test]
fn the_guard_lets_through_what_linux_sends_and_nothing_else() {
    k::guard().unwrap();
}

#[test]
fn messages_in_linux_order() {
    k::order().unwrap();
}

#[test]
fn the_tool_reads_with_the_block_asleep() {
    k::tool().unwrap();
}

#[test]
fn jobs_on_a_sleeping_block() {
    k::jobs().unwrap();
}

#[test]
fn info_register_read_wakes_the_block() {
    k::regs().unwrap();
}

#[test]
fn refusals_leave_the_clocks_and_the_driver() {
    k::refused().unwrap();
}

#[test]
fn stop_and_quiesce() {
    k::stop().unwrap();
}

#[test]
fn an_allow_steps_back_for_a_late_request() {
    e::allow_race().unwrap();
}

#[test]
fn a_step_on_a_sleeping_block_has_no_registers() {
    e::asleep().unwrap();
}

#[test]
fn a_late_answer_brings_the_messages_back() {
    e::late().unwrap();
}

#[test]
fn a_silent_transfer_keeps_the_page() {
    e::silent_metrics().unwrap();
}

#[test]
fn the_wiped_page_is_flushed_first() {
    e::flush_first().unwrap();
}

#[test]
fn the_tool_run_holds_a_backend_it_started() {
    e::tool_hold().unwrap();
}

#[test]
fn one_hook_on_the_last_start() {
    e::one_hook().unwrap();
}
