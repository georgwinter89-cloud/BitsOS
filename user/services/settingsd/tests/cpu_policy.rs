//! GP-CPU2: `kernel.cpu_policy` exists only where it does something (Intel with HWP and EPP, on BitsOS).

use settingsd::builtin;
use settingsd::value::Value;

const GAMING_PC: &str = "processor\t: 0\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t\t: 198\nflags\t\t: fpu hwp hwp_notify hwp_act_window hwp_epp hwp_pkg_req\n";
const MINI_PC: &str = "processor\t: 0\nvendor_id\t: AuthenticAMD\ncpu family\t: 25\nmodel\t\t: 117\nflags\t\t: fpu cppc\n";

#[test]
fn only_intel_with_hwp_epp() {
    assert!(builtin::cpu_policy_applies(GAMING_PC));
    assert!(!builtin::cpu_policy_applies(MINI_PC));
    assert!(!builtin::cpu_policy_applies(&GAMING_PC.replace(" hwp_epp", "")), "HWP without EPP");
    assert!(!builtin::cpu_policy_applies(&MINI_PC.replace("cppc", "hwp_epp")), "the flag alone on another vendor");
    assert!(!builtin::cpu_policy_applies(""));
}

#[test]
fn the_key_is_valid_and_absent_from_the_shared_list() {
    let d = builtin::cpu_policy();
    d.validate().unwrap();
    assert_eq!(d.default, Value::Text("balanced".into()));
    // On a Linux host (no /proc/bitsos) the list is the one every machine had before.
    assert!(builtin::machine().is_empty());
    assert!(!builtin::all().iter().any(|k| k.name == "kernel.cpu_policy"));
}
