//! Bidirectional speed probe used during connection setup.
//!
//! Flow:
//!   1. Host sends PROBE_BYTES worth of SPEED_PROBE chunks to the client.
//!   2. Client measures throughput + inter-chunk jitter, replies with SPEED_REPORT.
//!   3. Host decodes the report and returns SpeedStats to the caller.
//!
//! The same SpeedStats type is used on the client side (measured locally while
//! receiving the probe) so both sides can log and warn consistently.

use std::io::{self, Read, Write};
use std::time::Instant;

use super::proto::{self, msg, PROBE_BYTES, PROBE_CHUNK_DATA};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SpeedStats {
    /// Measured downstream throughput in bytes per second.
    pub throughput_bps: u64,
    /// Standard deviation of inter-chunk arrival delay, in milliseconds.
    /// High values indicate a jittery connection that needs a larger buffer.
    pub jitter_ms: f64,
}

impl SpeedStats {
    /// Recommended client-side buffer depth: 3σ jitter, clamped to [100, 2000] ms.
    pub fn recommended_buffer_ms(&self) -> u64 {
        ((self.jitter_ms * 3.0).ceil() as u64).clamp(100, 2_000)
    }

    /// True if the link is stable enough for comfortable streaming.
    /// Threshold: jitter < 50 ms and throughput ≥ 500 KB/s.
    pub fn is_stable(&self) -> bool {
        self.jitter_ms < 50.0 && self.throughput_bps >= 500_000
    }

    pub fn log(&self, label: &str) {
        println!(
            "[net] {} speed: {:.1} KB/s, jitter {:.1} ms → buffer {}ms{}",
            label,
            self.throughput_bps as f64 / 1024.0,
            self.jitter_ms,
            self.recommended_buffer_ms(),
            if self.is_stable() { "" } else { "  ⚠ UNSTABLE" },
        );
    }
}

// ── Host side ─────────────────────────────────────────────────────────────────

/// Send the probe to the client, then block until the client's SPEED_REPORT
/// arrives.  Returns stats decoded from the client's measurement.
pub fn host_send_probe(r: &mut impl Read, w: &mut impl Write) -> io::Result<SpeedStats> {
    let chunk_payload = PROBE_CHUNK_DATA;
    let total_chunks = (PROBE_BYTES + chunk_payload - 1) / chunk_payload;
    let pad = vec![0u8; chunk_payload];

    for seq in 0..total_chunks {
        let mut payload = Vec::with_capacity(8 + chunk_payload);
        payload.extend_from_slice(&(seq as u32).to_le_bytes());
        payload.extend_from_slice(&(total_chunks as u32).to_le_bytes());
        payload.extend_from_slice(&pad);
        proto::write_msg(w, msg::SPEED_PROBE, &payload)?;
    }

    let (ty, p) = proto::read_msg(r)?;
    if ty != msg::SPEED_REPORT {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected SPEED_REPORT"));
    }
    let report = proto::decode_speed_report(&p)?;

    Ok(SpeedStats {
        throughput_bps: throughput(report.total_bytes, report.elapsed_us),
        jitter_ms: report.jitter_us / 1000.0,
    })
}

// ── Client side ───────────────────────────────────────────────────────────────

/// Receive all probe chunks from the host, measure them, send SPEED_REPORT,
/// and return the locally-measured stats (so the client can display them too).
pub fn client_receive_probe(r: &mut impl Read, w: &mut impl Write) -> io::Result<SpeedStats> {
    let start = Instant::now();
    let mut arrivals_us: Vec<u64> = Vec::new();
    let mut total_bytes: u64 = 0;

    loop {
        let (ty, payload) = proto::read_msg(r)?;
        if ty != msg::SPEED_PROBE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected SPEED_PROBE"));
        }
        if payload.len() < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "probe chunk too short"));
        }

        let seq   = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let total = u32::from_le_bytes(payload[4..8].try_into().unwrap());
        total_bytes += (payload.len() - 8) as u64;
        arrivals_us.push(start.elapsed().as_micros() as u64);

        if seq == total - 1 { break; }
    }

    let elapsed_us = start.elapsed().as_micros() as u64;
    let jitter_us = stddev_us(&arrivals_us);

    proto::write_msg(
        w,
        msg::SPEED_REPORT,
        &proto::encode_speed_report(total_bytes, elapsed_us, jitter_us),
    )?;

    Ok(SpeedStats {
        throughput_bps: throughput(total_bytes, elapsed_us),
        jitter_ms: jitter_us / 1000.0,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn throughput(bytes: u64, elapsed_us: u64) -> u64 {
    if elapsed_us == 0 { return 0; }
    (bytes as u128 * 1_000_000 / elapsed_us as u128) as u64
}

/// Standard deviation of inter-arrival intervals (in microseconds).
fn stddev_us(arrivals: &[u64]) -> f64 {
    if arrivals.len() < 2 { return 0.0; }
    let deltas: Vec<f64> = arrivals.windows(2)
        .map(|w| (w[1] - w[0]) as f64)
        .collect();
    let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
    let variance = deltas.iter()
        .map(|&d| { let diff = d - mean; diff * diff })
        .sum::<f64>()
        / deltas.len() as f64;
    variance.sqrt()
}
