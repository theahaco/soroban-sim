// Any per-line exact-apply feature enables the shared `exact_apply` cfg.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(exact_apply)");
    if ["EXACT_APPLY", "EXACT_APPLY_P27", "EXACT_APPLY_P30"]
        .iter()
        .any(|feature| std::env::var_os(format!("CARGO_FEATURE_{feature}")).is_some())
    {
        println!("cargo::rustc-cfg=exact_apply");
    }
}
