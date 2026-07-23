fn main() {
    let musl = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "musl";
    let musl_v1_2_3 = std::env::var("RUST_LIBC_UNSTABLE_MUSL_V1_2_3").is_ok();
    println!("cargo:rerun-if-env-changed=RUST_LIBC_UNSTABLE_MUSL_V1_2_3");
    println!("cargo:rustc-check-cfg=cfg(musl_v1_2_3)");
    if musl && musl_v1_2_3 {
        println!("cargo:rustc-cfg=musl_v1_2_3");
    }

    println!("cargo:rustc-check-cfg=cfg(syscall_pipe2)");
    println!("cargo:rustc-check-cfg=cfg(syscall_accept4)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if [
        "linux",
        "android",
        "freebsd",
        "illumos",
        "solaris",
        "emscripten",
        "hurd",
        "redox",
        "netbsd",
        "cygwin",
    ]
    .contains(&target_os.as_str())
    {
        println!("cargo:rustc-cfg=syscall_pipe2");
    }
    if [
        "freebsd",
        "netbsd",
        "emscripten",
        "fuchsia",
        "solaris",
        "illumos",
        "linux",
    ]
    .contains(&target_os.as_str())
    {
        println!("cargo:rustc-cfg=syscall_accept4");
    }
}
