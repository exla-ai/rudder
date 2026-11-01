use std::{env, path::PathBuf, process::Command};
fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("actual Cargo output"));
    let object = out.join("hello.o");
    assert!(Command::new("/usr/bin/gcc").args(["-DC1_LIBRARY_ONLY", "-c", "hello.c", "-o"]).arg(&object).status().unwrap().success());
    let archive = out.join("libc1.a");
    assert!(Command::new("/usr/bin/ar").arg("rcs").arg(&archive).arg(&object).status().unwrap().success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=c1");
    println!("cargo:rerun-if-changed=hello.c");
}
