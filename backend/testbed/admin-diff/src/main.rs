//! The differential replay runner (testbed/README.md).
//!
//! Brings up the corpus (generated sweep + curated), waits for both
//! backends, replays every case against both, and writes the JSONL report
//! plus the summary. Exit code 2 when a backend never came up.

use std::time::{Duration, Instant};

use admin_diff::compare::{judge, Kind, Verdict};
use admin_diff::corpus::{load_curated, load_exemptions, sweep, Case};
use admin_diff::report::{self, Entry, Side};
use admin_diff::ProbeResult;

struct Args {
    go: String,
    rust: String,
    corpus_dir: String,
    exemptions: String,
    out: String,
    wait: u64,
}

fn usage() -> ! {
    eprintln!(
        "usage: admin-diff --go URL --rust URL [--corpus DIR=testbed/corpus] \
         [--exemptions FILE=testbed/exemptions.json] [--out FILE=testbed/reports/report.jsonl] \
         [--wait SECS=180]"
    );
    std::process::exit(1);
}

fn parse_args() -> Args {
    let mut args = Args {
        go: String::new(),
        rust: String::new(),
        corpus_dir: "testbed/corpus".into(),
        exemptions: "testbed/exemptions.json".into(),
        out: "testbed/reports/report.jsonl".into(),
        wait: 180,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let Some(value) = it.next() else {
            eprintln!("missing value for {flag}");
            usage();
        };
        match flag.as_str() {
            "--go" => args.go = value,
            "--rust" => args.rust = value,
            "--corpus" => args.corpus_dir = value,
            "--exemptions" => args.exemptions = value,
            "--out" => args.out = value,
            "--wait" => args.wait = value.parse().unwrap_or(180),
            _ => usage(),
        }
    }
    if args.go.is_empty() || args.rust.is_empty() {
        usage();
    }
    args
}

/// Waits until the endpoint answers any HTTP status, or the deadline
/// passes (returns false — the run then records every case against that
/// side as unreachable).
fn wait_up(client: &reqwest::blocking::Client, base: &str, wait: u64) -> bool {
    if wait == 0 {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(wait);
    while Instant::now() < deadline {
        let probe = client
            .get(format!("{base}/"))
            .timeout(Duration::from_secs(3))
            .send();
        if probe.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

/// Probes one backend with one case.
fn probe(client: &reqwest::blocking::Client, base: &str, case: &Case) -> ProbeResult {
    let Ok(method) = reqwest::Method::from_bytes(case.method.as_bytes()) else {
        return ProbeResult::down();
    };
    let mut req = client.request(method, format!("{base}{}", case.path));
    for (name, value) in &case.headers {
        req = req.header(name.as_str(), value.as_str());
    }
    if let Some(body) = &case.body {
        req = req
            .header("content-type", "application/json")
            .body(body.clone());
    }
    let Ok(resp) = req.send() else {
        return ProbeResult::down();
    };
    let status = resp.status().as_u16();
    // The cors capture exists only for the Cors kind so the report never
    // carries unrelated headers.
    let cors = if case.kind == Kind::Cors {
        let names = [
            "access-control-allow-origin",
            "access-control-allow-methods",
            "access-control-allow-credentials",
        ];
        Some(
            names
                .iter()
                .map(|n| {
                    (
                        (*n).to_string(),
                        resp.headers()
                            .get(*n)
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string()),
                    )
                })
                .collect(),
        )
    } else {
        None
    };
    let body = resp.bytes().ok().map(|b| b.to_vec());
    ProbeResult {
        reachable: true,
        status: Some(status),
        body,
        cors,
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Routing => "Routing",
        Kind::EnvelopeExact => "EnvelopeExact",
        Kind::EnvelopeShape => "EnvelopeShape",
        Kind::Cors => "Cors",
        Kind::Pending => "Pending",
    }
}

fn verdict_name(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Ok => "Ok",
        Verdict::Fail => "Fail",
        Verdict::Exempt => "Exempt",
        Verdict::Pending => "Pending",
        Verdict::Unreachable => "Unreachable",
    }
}

fn main() {
    let args = parse_args();
    let exemptions = load_exemptions(&args.exemptions);
    let sweep_cases = sweep();
    let sweep_count = sweep_cases.len();
    let curated = load_curated(&args.corpus_dir);
    let curated_count = curated.len();
    let mut cases = sweep_cases;
    cases.extend(curated);
    eprintln!(
        "corpus: {} cases (sweep {sweep_count} + curated {curated_count}), exempt classes: {}",
        cases.len(),
        exemptions.len()
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        // The rig talks to loopback only; a system proxy in the middle
        // rewrites answers (observed: phantom 400s on gated probes) and
        // poisons the differential.
        .no_proxy()
        .build()
        .expect("blocking client");
    let go_up = wait_up(&client, &args.go, args.wait);
    let rust_up = wait_up(&client, &args.rust, args.wait);
    eprintln!(
        "go backend: {} | rust backend: {}",
        if go_up { "up" } else { "DOWN" },
        if rust_up { "up" } else { "DOWN" }
    );
    let mut entries = Vec::with_capacity(cases.len());
    for (i, case) in cases.iter().enumerate() {
        let go = probe(&client, &args.go, case);
        let rust = probe(&client, &args.rust, case);
        let (verdict, detail) = judge(case.kind, &case.class, &go, &rust, &exemptions);
        entries.push(Entry {
            id: case.id.clone(),
            class: case.class.clone(),
            kind: kind_name(case.kind).into(),
            verdict: verdict_name(verdict).into(),
            go: Side::of(&go),
            rust: Side::of(&rust),
            detail,
        });
        if i % 50 == 0 {
            eprintln!("... {}/{}", i, cases.len());
        }
    }
    report::write(&args.out, &entries);
    report::summary(&entries);
    if !go_up || !rust_up {
        std::process::exit(2);
    }
}
