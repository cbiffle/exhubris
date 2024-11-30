use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=sys-idl.kdl");
    let iface = tools::idl::load_interface("sys-idl.kdl").unwrap();
    let client = tools::idl::codegen::generate_server(&iface).unwrap();
    let client = tools::idl::codegen::format_code(&client);
    let mut outpath = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    outpath.push("generated_server.rs");

    std::fs::write(&outpath, client).unwrap();

}
