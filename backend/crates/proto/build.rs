//! admin-proto build — the config face. The engine (the buf-built
//! annotated closure, the filtered protox types face, prost+pbjson
//! generation, and the gen-http route surface) lives in
//! `rushwind-proto-build`; this file carries the deployment's knobs.

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/auth_free.rs"));

fn main() -> Result<(), Box<dyn std::error::Error>> {
    rushwind_proto_build::run(rushwind_proto_build::Build {
        manifest_dir: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        dep_data_files: &["pagination/v1/pagination.proto"],
        auth_free: AUTH_FREE,
        redact_plan_expr: Some("crate::redact_plan()"),
        proto_module_path: "crate::proto",
        pool_expr: "crate::pool()",
        tonic: false,
        faces: &[rushwind_proto_build::Face {
            label: "admin",
            module_file: "admin_gen.rs",
            root_prefix: None,
            module_path: None,
        }],
    })
}
