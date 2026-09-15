//! admin-proto build.
//!
//! Compiles the synced admin contract tree (`backend/api/protos`, checksummed
//! by `backend/api/sync-protos.sh`) together with its vendored dependencies
//! (`backend/api/third_party`, see its PROVENANCE.md) into BOTH faces of the
//! contract:
//!
//! * the annotated descriptor set — the FULL compile closure including the
//!   annotation declarations (google.api.http, errors.code, redact,
//!   validate, gnostic) — produced by `protoc`, NOT protox: protox's
//!   serializer drops custom-option bytes, which are the entire point of
//!   this set. `admin-gen` parses it for routes/error tables/binding plans;
//!   the runtime pool decodes it as the schema surface for protojson
//!   serialization and form binding. Requires `protoc` on PATH.
//! * the Rust types — prost + pbjson, from a FILTERED descriptor set built
//!   by protox: only data-carrying files (the admin contract files, the
//!   pagination messages, the well-known types referenced as field
//!   types). Annotation-only files are removed so prost/pbjson never emit
//!   types for them (and so `google/protobuf/descriptor.proto` — pulled in
//!   by the gnostic annotations — never enters type generation, where the
//!   `.google.protobuf` extern would produce unresolvable references).
//!
//! Kept files additionally have their `dependency` lists pruned to kept
//! files: option-bearing imports carry no type references, so the dangling
//! edges are harmless for codegen while keeping the synced protos untouched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use protox::prost::Message as _;

// The deployment's whitelist table, single-sourced from src/auth_free.rs —
// the same file the lib re-exports (included at build-script scope so the
// generator config can consume it).
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/auth_free.rs"));

/// Vendored data-carrying files (the paging envelope). These are real
/// runtime types (List request inputs) and participate in type generation.
const VENDORED_DATA_FILES: [&str; 1] = ["pagination/v1/pagination.proto"];

/// Well-known types referenced as field types by the contract tree
/// (import survey: empty 67, timestamp 50, field_mask 32, duration 2).
/// Externed to `pbjson_types` by the prost config below; kept in the filtered
/// descriptor set so references resolve, excluded from pbjson output.
const WELL_KNOWN_PREFIX: &str = "google/protobuf/";

/// The full closure set is only written for admin-gen; the types set feeds
/// prost/pbjson. Returns true for files that may enter the types set.
fn is_types_kept(name: &str) -> bool {
    let is_admin = name.starts_with("access_key/")
        || name.starts_with("admin/")
        || name.starts_with("audit/")
        || name.starts_with("authentication/")
        || name.starts_with("config/")
        || name.starts_with("dict/")
        || name.starts_with("identity/")
        || name.starts_with("internal_message/")
        || name.starts_with("notification_channel/")
        || name.starts_with("online_session/")
        || name.starts_with("permission/")
        || name.starts_with("redis_cache/")
        || name.starts_with("script/")
        || name.starts_with("server_monitor/")
        || name.starts_with("storage/")
        || name.starts_with("task/");
    is_admin || VENDORED_DATA_FILES.contains(&name) || name.starts_with(WELL_KNOWN_PREFIX)
}

fn is_well_known(name: &str) -> bool {
    name.starts_with(WELL_KNOWN_PREFIX)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // The crate sits at backend/crates/proto — two levels up is backend/.
    let backend_root = manifest_dir.join("../..");
    let proto_root = backend_root.join("api/protos");
    let third_party_root = backend_root.join("api/third_party");

    // Admin contract files + vendored data files: the codegen request set.
    let mut compile_files: Vec<PathBuf> = Vec::new();
    collect_protos(&proto_root, &mut compile_files)?;
    if compile_files.is_empty() {
        return Err("no admin protos found under backend/api/protos".into());
    }
    for rel in VENDORED_DATA_FILES {
        let p = third_party_root.join(rel);
        if !p.is_file() {
            return Err(format!("vendored data file missing: {}", p.display()).into());
        }
        compile_files.push(p);
    }

    let includes = [proto_root.as_path(), third_party_root.as_path()];

    // 1. The annotated full closure via protoc (option bytes preserved) —
    //    the generator and runtime-pool input.
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let annotated_path = out_dir.join("annotated_descriptor.bin");
    let mut cmd = Command::new("protoc");
    cmd.arg("--include_imports")
        .arg(format!("--descriptor_set_out={}", annotated_path.display()))
        .arg("-I")
        .arg(&proto_root)
        .arg("-I")
        .arg(&third_party_root);
    for input in &compile_files {
        cmd.arg(input);
    }
    let output = cmd.output().map_err(|e| format!("protoc: {e}"))?;
    if !output.status.success() {
        return Err(format!("protoc failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    let annotated_size = fs::metadata(&annotated_path).map(|m| m.len()).unwrap_or(0);

    // 2. The types face via protox: the full closure (option bytes unused
    //    here) filtered to data-carrying files.
    let full_fds = protox::compile(&compile_files, includes)?;

    // Filtered set for type generation: drop annotation-only files, prune
    // dependency edges to kept files.
    let mut types_fds = full_fds.clone();
    types_fds.file.retain(|f| is_types_kept(f.name()));
    for f in &mut types_fds.file {
        let deps: Vec<String> = f.dependency.to_vec();
        f.dependency = deps
            .into_iter()
            .filter(|d| is_types_kept(d.as_str()))
            .collect();
    }

    // Filtered closure → types_descriptor.bin (prost + pbjson input).
    let types_path = out_dir.join("types_descriptor.bin");
    fs::write(&types_path, types_fds.encode_to_vec())?;

    // Sweep stale codegen artifacts (earlier runs may have written packages
    // that the filter has since dropped).
    for entry in fs::read_dir(&out_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "rs").unwrap_or(false) {
            let _ = fs::remove_file(&path);
        }
    }

    eprintln!(
        "[admin-proto] annotated closure: {annotated_size} bytes (protoc); types set: {} files (annotation declarations dropped)",
        types_fds.file.len()
    );

    // prost type generation, reading the FILTERED descriptor set verbatim.
    // skip_protoc_run is essential: without it prost-build would invoke
    // protoc on the request files and OVERWRITE types_descriptor.bin with
    // the full closure, resurrecting the annotation packages.
    let mut config = prost_build::Config::new();
    config
        .file_descriptor_set_path(&types_path)
        .skip_protoc_run()
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .include_file("proto_include.rs");
    config.compile_protos(&compile_files, &includes)?;

    // pbjson serde impls: data packages of the filtered set only. The package
    // list is derived from the set itself (dir names do not map 1:1 to
    // package names — e.g. resource/ declares permission.service.v1);
    // well-knowns are externed and excluded.
    let mut packages: Vec<String> = types_fds
        .file
        .iter()
        .filter(|f| !is_well_known(f.name()))
        .map(|f| format!(".{}", f.package()))
        .collect();
    packages.sort();
    packages.dedup();
    let pkg_refs: Vec<&str> = packages.iter().map(|s| s.as_str()).collect();
    pbjson_build::Builder::new()
        .register_descriptors(&fs::read(&types_path)?)?
        .build(&pkg_refs)?;

    // 3. The generated route/binding/trait/mount surface: the framework
    //    generator (rushwind-gen-http) over the annotated closure. The
    //    emitted module lives in THIS crate, so generated types resolve
    //    through `crate::proto` and the pool through `crate::pool()`.
    let annotated = fs::read(&annotated_path)?;
    let cfg = rushwind_gen_http::CodegenConfig {
        proto_module_path: "crate::proto",
        pool_expr: "crate::pool()",
        auth_free: AUTH_FREE,
    };
    let src = match rushwind_gen_http::generate_from_bytes(&annotated, &cfg) {
        Ok(src) => src,
        Err(e) => panic!("gen-rust code generation failed: {e}"),
    };
    fs::write(out_dir.join("admin_gen.rs"), src)?;

    // Re-run on any contract change (and on the auth-free table).
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("src/auth_free.rs").display()
    );
    println!("cargo:rerun-if-changed={}", third_party_root.display());
    println!("cargo:rerun-if-changed={}", proto_root.display());

    Ok(())
}

fn collect_protos(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_protos(&path, out)?;
        } else if path.extension().map(|e| e == "proto").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}
