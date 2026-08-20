//! Benchmark client (SPEC §4, §9): a real client of the same order-entry
//! socket any other connection uses. Spawns the exact daemon wiring
//! `main.rs` runs (`bin::run`) on a background thread bound to a fresh
//! socket path, then connects to it and drives the coordinated-omission-
//! safe HDR latency measurement in `harness.rs` against the mixed
//! workload in `workload.rs`.
//!
//! Usage: `cargo run -p bench --release -- --messages 1000000
//! [--rate 200000] [--warmup 50000] [--new-pct 70] [--cancel-pct 20]
//! [--cross-pct 10] [--risk-config risk-bench.toml]`

mod harness;
mod workload;

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use harness::RunConfig;
use workload::{Mix, Workload};

const DEFAULT_MESSAGES: u64 = 1_000_000;
const DEFAULT_RATE_PER_SEC: u64 = 200_000;
const DEFAULT_WARMUP: u64 = 50_000;
const DEFAULT_RISK_CONFIG: &str = "risk-bench.toml";

struct Args {
    messages: u64,
    rate_per_sec: u64,
    warmup: u64,
    mix: Mix,
    risk_config_path: PathBuf,
}

fn parse_args() -> Args {
    let mut messages = DEFAULT_MESSAGES;
    let mut rate_per_sec = DEFAULT_RATE_PER_SEC;
    let mut warmup = DEFAULT_WARMUP;
    let mut new_pct = 70u32;
    let mut cancel_pct = 20u32;
    let mut cross_pct = 10u32;
    let mut risk_config_path = PathBuf::from(DEFAULT_RISK_CONFIG);

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let next = |i: &mut usize, flag: &str| -> String {
        *i += 1;
        args.get(*i).cloned().unwrap_or_else(|| {
            eprintln!("{flag} requires a value");
            std::process::exit(1);
        })
    };
    let parse_u64 = |flag: &str, value: &str| -> u64 {
        value.parse().unwrap_or_else(|_| {
            eprintln!("{flag}: {value:?} is not a valid non-negative integer");
            std::process::exit(1);
        })
    };
    let parse_u32 = |flag: &str, value: &str| -> u32 {
        value.parse().unwrap_or_else(|_| {
            eprintln!("{flag}: {value:?} is not a valid non-negative integer");
            std::process::exit(1);
        })
    };

    while i < args.len() {
        match args[i].as_str() {
            "--messages" => messages = parse_u64("--messages", &next(&mut i, "--messages")),
            "--rate" => rate_per_sec = parse_u64("--rate", &next(&mut i, "--rate")),
            "--warmup" => warmup = parse_u64("--warmup", &next(&mut i, "--warmup")),
            "--new-pct" => new_pct = parse_u32("--new-pct", &next(&mut i, "--new-pct")),
            "--cancel-pct" => cancel_pct = parse_u32("--cancel-pct", &next(&mut i, "--cancel-pct")),
            "--cross-pct" => cross_pct = parse_u32("--cross-pct", &next(&mut i, "--cross-pct")),
            "--risk-config" => risk_config_path = PathBuf::from(next(&mut i, "--risk-config")),
            other => {
                eprintln!("unrecognized argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    if rate_per_sec == 0 {
        eprintln!("--rate must be positive");
        std::process::exit(1);
    }
    if warmup > messages {
        eprintln!("--warmup ({warmup}) cannot exceed --messages ({messages})");
        std::process::exit(1);
    }
    let mix = match (Mix {
        new_pct,
        cancel_pct,
        cross_pct,
    })
    .validate()
    {
        Ok(mix) => mix,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    Args {
        messages,
        rate_per_sec,
        warmup,
        mix,
        risk_config_path,
    }
}

fn temp_socket_path(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        "/tmp/bench-{label}-{}-{n}.sock",
        std::process::id()
    ))
}

fn connect_with_retry(path: &PathBuf) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return stream,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                eprintln!("failed to connect to {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }
}

fn main() {
    let args = parse_args();

    let order_entry_path = temp_socket_path("oe");
    let market_data_path = temp_socket_path("md");
    {
        let order_entry_path = order_entry_path.clone();
        let market_data_path = market_data_path.clone();
        let risk_config_path = args.risk_config_path.clone();
        std::thread::spawn(move || {
            bin::run(
                &order_entry_path,
                &market_data_path,
                &risk_config_path,
                None,
            )
        });
    }

    println!("bench: risk config {}", args.risk_config_path.display());
    println!(
        "bench: {} messages at {} msg/s, {} warmup, mix new/cancel/cross = {}/{}/{}",
        args.messages,
        args.rate_per_sec,
        args.warmup,
        args.mix.new_pct,
        args.mix.cancel_pct,
        args.mix.cross_pct
    );

    let stream = connect_with_retry(&order_entry_path);
    let workload = Workload::new(args.mix, 0x9E37_79B9_7F4A_7C15);

    let report = harness::run(
        stream,
        &workload,
        RunConfig {
            messages: args.messages,
            rate_per_sec: args.rate_per_sec,
            warmup: args.warmup,
        },
    );

    print_report(&report);
}

fn print_report(report: &harness::RunReport) {
    if report.unmatched() > 0 {
        println!(
            "WARNING: {} of {} sent messages never received a matched reply (idle timeout reached) -- \
             the histogram below reflects only the {} that did.",
            report.unmatched(),
            report.sent,
            report.matched
        );
    }

    println!("--- counts ---");
    println!("sent:              {}", report.sent);
    println!("matched:           {}", report.matched);
    println!("unmatched:         {}", report.unmatched());
    println!("warmup excluded:   {}", report.warmup_excluded);
    println!("histogram samples: {}", report.histogram_samples());

    println!("--- latency (ingress -> execution report emitted) ---");
    let h = &report.histogram;
    println!(
        "p50:    {:>10.3} us",
        h.value_at_quantile(0.50) as f64 / 1_000.0
    );
    println!(
        "p99:    {:>10.3} us",
        h.value_at_quantile(0.99) as f64 / 1_000.0
    );
    println!(
        "p99.9:  {:>10.3} us",
        h.value_at_quantile(0.999) as f64 / 1_000.0
    );
    println!(
        "p99.99: {:>10.3} us",
        h.value_at_quantile(0.9999) as f64 / 1_000.0
    );
    println!("max:    {:>10.3} us", h.max() as f64 / 1_000.0);

    println!("--- throughput ---");
    let secs = report.wall_clock.as_secs_f64();
    println!("wall clock: {secs:.3} s");
    println!("sustained:  {:.0} msg/s", report.sent as f64 / secs);
}
