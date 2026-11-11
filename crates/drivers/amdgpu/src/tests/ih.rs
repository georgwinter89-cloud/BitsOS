//! GX-7n: the IH ring's checks on the host (`ktests_hw_ih`).

use crate::ktests_hw_ih as k;

#[test]
fn the_entry_the_table_the_registers() {
    k::check_format().unwrap();
}

#[test]
fn the_watch() {
    k::check_watch().unwrap();
}

#[test]
fn the_reader() {
    k::check_reader().unwrap();
}

#[test]
fn overflow() {
    k::check_overflow().unwrap();
}

#[test]
fn flood() {
    k::check_flood().unwrap();
}

#[test]
fn the_chip_side_in_linux_order() {
    k::check_chip_side().unwrap();
}

#[test]
fn rearm_and_the_display_masked() {
    k::check_rearm_masked().unwrap();
}

#[test]
fn jobs_with_interrupts() {
    k::check_jobs().unwrap();
}

#[test]
fn lost_interrupts_hang_nothing() {
    k::check_lost().unwrap();
}

#[test]
fn an_entry_counted_after_the_look_is_not_lost() {
    k::check_fast_reader().unwrap();
}

#[test]
fn the_buffer_is_in_a_piece_of_its_own() {
    k::check_bo_piece().unwrap();
}

#[test]
fn the_glue_only_the_chip_runs() {
    k::check_live_glue().unwrap();
}

#[test]
fn the_job_threads_look() {
    k::check_look().unwrap();
}

#[test]
fn a_fault_raises() {
    k::check_fault().unwrap();
}

#[test]
fn a_hang_on_interrupts() {
    k::check_hang().unwrap();
}

#[test]
fn stop_and_hand_over() {
    k::check_cycles().unwrap();
}

#[test]
fn the_tool_arguments() {
    use crate::psplive::{Action, parse_args};
    let a = |s: &str| parse_args(&mcp::Json::parse(s).unwrap()).map(|x| x.action);
    assert_eq!(a(r#"{"until":"irq_on"}"#), Ok(Action::Irq(true)));
    assert_eq!(a(r#"{"until":"irq_poll"}"#), Ok(Action::Irq(false)));
    assert_eq!(a(r#"{"until":"irq_load"}"#), Ok(Action::IrqLoad));
}
