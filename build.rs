use std::path::PathBuf;

fn main() {
    let plist = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("Info.plist");
    println!("cargo:rerun-if-changed=Info.plist");
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}
