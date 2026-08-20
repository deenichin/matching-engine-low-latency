//! Coordinated-omission-safe HDR latency measurement over a real UDS
//! order-entry connection (SPEC §4, §9). See the module-level notes in
//! `workload.rs` for the traffic this drives.
//!
//! The core discipline: every message's latency is measured against its
//! *intended* send time on a fixed schedule computed once up front, never
//! against when it was actually sent or when the previous reply arrived.
//! The sender never waits for a reply before proceeding to the next
//! scheduled slot -- if the system falls behind, the sender keeps pacing
//! against the original schedule regardless, so the resulting queueing
//! delay shows up in the measured latencies instead of being silently
//! absorbed by a sender that slows down to match. A harness that derives
//! the next send time from the last reply's arrival hides exactly the
//! thing coordinated-omission handling exists to expose.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use core::event::Event;
use core::types::{AccountId, OrderId};
use hdrhistogram::Histogram;

use crate::workload::Workload;

/// How long the reader waits for a new correlation match after the sender
/// has finished sending, before giving up. A concrete value, not
/// "generous": 30s of no forward progress at all on a local UDS socket
/// means something is genuinely stuck, not just slow.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Read-timeout granularity for the reader's poll loop -- just how often
/// it re-checks the termination conditions, not a measurement parameter.
const READER_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Histogram bounds: 1ns to 60s, enough headroom for a genuinely stuck
/// reply without silently clipping a real (if bad) tail latency.
const HISTOGRAM_MIN_NS: u64 = 1;
const HISTOGRAM_MAX_NS: u64 = 60_000_000_000;
const HISTOGRAM_SIGFIGS: u8 = 3;

pub struct RunConfig {
    pub messages: u64,
    pub rate_per_sec: u64,
    pub warmup: u64,
}

pub struct RunReport {
    pub sent: u64,
    pub matched: u64,
    pub warmup_excluded: u64,
    pub histogram: Histogram<u64>,
    pub wall_clock: Duration,
}

impl RunReport {
    pub fn unmatched(&self) -> u64 {
        self.sent - self.matched
    }

    pub fn histogram_samples(&self) -> u64 {
        self.matched - self.warmup_excluded
    }
}

fn event_correlation_id(event: &Event) -> Option<(AccountId, OrderId)> {
    match *event {
        Event::Accepted {
            account_id,
            order_id,
            ..
        }
        | Event::Rejected {
            account_id,
            order_id,
            ..
        }
        | Event::Filled {
            account_id,
            order_id,
            ..
        }
        | Event::Cancelled {
            account_id,
            order_id,
        }
        | Event::Replaced {
            account_id,
            order_id,
            ..
        } => Some((account_id, order_id)),
        Event::Trade { .. } | Event::BookUpdate { .. } => None,
    }
}

/// Runs the fixed-rate, coordinated-omission-safe measurement loop over
/// `stream` (an already-connected order-entry socket) and returns the
/// full report -- hard counts always included, not only on an incomplete
/// run, so a lossy run can never look identical to a clean one.
pub fn run(stream: UnixStream, workload: &Workload, config: RunConfig) -> RunReport {
    assert!(config.rate_per_sec > 0, "rate must be positive");

    let mut sender_stream = stream
        .try_clone()
        .expect("failed to clone the bench socket for the sender thread");
    let mut reader_stream = stream;
    reader_stream
        .set_read_timeout(Some(READER_POLL_INTERVAL))
        .expect("failed to set the reader's poll timeout");

    let pending: Mutex<HashMap<(AccountId, OrderId), Instant>> = Mutex::new(HashMap::new());
    let matched_count = AtomicU64::new(0);
    let sender_done = AtomicBool::new(false);
    let histogram: Mutex<Histogram<u64>> = Mutex::new(
        Histogram::new_with_bounds(HISTOGRAM_MIN_NS, HISTOGRAM_MAX_NS, HISTOGRAM_SIGFIGS)
            .expect("HISTOGRAM_MIN_NS/MAX_NS/SIGFIGS are valid histogram bounds"),
    );

    let interval = Duration::from_secs_f64(1.0 / config.rate_per_sec as f64);
    let start = Instant::now();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..config.messages {
                let intended = start + interval * (i as u32);
                let now = Instant::now();
                if intended > now {
                    std::thread::sleep(intended - now);
                }

                let (key, cmd) = workload.next_command();
                pending
                    .lock()
                    .expect("pending mutex poisoned")
                    .insert(key, intended);

                let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
                let len = wire::encode_command(&cmd, &mut buf);
                // A write failure here means the connection died -- the
                // reader's idle timeout below is what notices and ends the
                // run; nothing useful to do differently here mid-loop.
                let _ = sender_stream.write_all(&buf[..len]);
            }
            sender_done.store(true, Ordering::Release);
        });

        scope.spawn(|| {
            let mut framer = wire::Framer::new();
            let mut read_buf = [0u8; 65536];
            let mut last_match = Instant::now();

            loop {
                match reader_stream.read(&mut read_buf) {
                    Ok(0) => break, // connection closed
                    Ok(n) => {
                        framer.feed(&read_buf[..n]);
                        let _ = framer.drain_frames(|frame| {
                            let Ok((_seq, event)) = wire::decode_event(frame) else {
                                return;
                            };
                            workload.observe(&event);
                            let Some(id) = event_correlation_id(&event) else {
                                return;
                            };
                            let intended =
                                pending.lock().expect("pending mutex poisoned").remove(&id);
                            let Some(intended) = intended else {
                                // Not the first reply for this command (a
                                // crossing order's Filled followed by its
                                // own Accepted), or a reply for an order
                                // this run's own bookkeeping already
                                // considers settled -- bookkeeping only,
                                // not a second latency sample.
                                return;
                            };
                            let now = Instant::now();
                            let latency_ns =
                                now.saturating_duration_since(intended).as_nanos() as u64;
                            let count = matched_count.fetch_add(1, Ordering::AcqRel) + 1;
                            if count > config.warmup {
                                histogram
                                    .lock()
                                    .expect("histogram mutex poisoned")
                                    .record(latency_ns.clamp(HISTOGRAM_MIN_NS, HISTOGRAM_MAX_NS))
                                    .expect("latency clamped within histogram bounds");
                            }
                            last_match = Instant::now();
                        });
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => break,
                }

                let done_sending = sender_done.load(Ordering::Acquire);
                let matched = matched_count.load(Ordering::Acquire);
                if done_sending && matched >= config.messages {
                    break;
                }
                if done_sending && last_match.elapsed() > IDLE_TIMEOUT {
                    break;
                }
            }
        });
    });

    let wall_clock = start.elapsed();
    let matched = matched_count.load(Ordering::Acquire);
    RunReport {
        sent: config.messages,
        matched,
        warmup_excluded: config.warmup.min(matched),
        histogram: histogram.into_inner().expect("histogram mutex poisoned"),
        wall_clock,
    }
}
