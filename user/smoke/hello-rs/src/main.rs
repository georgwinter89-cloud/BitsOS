// The G0 smoke test checks this exact output (kernel ktest smoke.hello_rs).
fn main() {
    let v: Vec<u32> = (1..=4).collect();
    println!("hello from Rust std on BitsOS V2, sum={}", v.iter().sum::<u32>());
}
