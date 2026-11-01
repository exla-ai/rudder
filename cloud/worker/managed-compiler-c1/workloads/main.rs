unsafe extern "C" { fn c1_add(a: i32, b: i32) -> i32; }
fn main() { assert_eq!(unsafe { c1_add(19, 23) }, 42); println!("c1-rust-executable-ok"); }
