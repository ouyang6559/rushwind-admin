//! The report: one JSON line per case plus a stdout summary.

use std::io::Write as _;

use crate::ProbeResult;

/// One report line. Body bytes are never echoed for Ok/Pending/Exempt
/// verdicts; Fail details carry the divergence evidence.
#[derive(serde::Serialize)]
pub struct Entry {
    pub id: String,
    pub class: String,
    pub kind: String,
    pub verdict: String,
    pub go: Side,
    pub rust: Side,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(serde::Serialize)]
pub struct Side {
    pub reachable: bool,
    pub status: Option<u16>,
    pub len: Option<usize>,
}

impl Side {
    /// The report-facing projection of a probe result.
    pub fn of(p: &ProbeResult) -> Self {
        Self {
            reachable: p.reachable,
            status: p.status,
            len: p.body.as_ref().map(|b| b.len()),
        }
    }
}

/// Writes the JSONL report, creating the parent directory.
pub fn write(path: &str, entries: &[Entry]) {
    let Some(dir) = std::path::Path::new(path).parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    if let Ok(mut file) = std::fs::File::create(path) {
        for entry in entries {
            if let Ok(line) = serde_json::to_string(entry) {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

/// Prints the verdict histogram and every Fail case's detail line.
pub fn summary(entries: &[Entry]) {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for e in entries {
        *counts.entry(e.verdict.clone()).or_default() += 1;
    }
    println!("=== differential summary ===");
    for (verdict, count) in &counts {
        println!("{verdict}: {count}");
    }
    let fails: Vec<&Entry> = entries.iter().filter(|e| e.verdict == "Fail").collect();
    if !fails.is_empty() {
        println!("--- FAIL details ({}) ---", fails.len());
        for f in fails {
            println!(
                "{} [{}] {}",
                f.id,
                f.class,
                f.detail.clone().unwrap_or_default()
            );
        }
    }
    let pending = entries.iter().filter(|e| e.verdict == "Pending").count();
    if pending > 0 {
        println!("--- pending (recorded, unasserted; Rust side is stubs): {pending} ---");
    }
}
