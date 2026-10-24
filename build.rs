fn main() {
    // A shipcheck host check builds and runs the tests jailed, and macOS refuses a jail inside a jail
    println!("cargo::rerun-if-env-changed=SHIPCHECK_JAIL");
    println!("cargo::rustc-check-cfg=cfg(jail)");

    if std::env::var_os("SHIPCHECK_JAIL").is_some() {
        println!("cargo::rustc-cfg=jail");
    }
}
