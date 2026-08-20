//! Extracts complete, fixed-length frames from an accumulating byte
//! stream (SPEC §3). `SOCK_STREAM` gives no message boundaries — a single
//! `read()` may return a partial message, several messages, or both — so
//! the framer buffers what it's given and hands back only whole frames,
//! retaining any trailing partial one for the next `feed`.

use core::error::RejectReason;

use crate::tag::message_len;

/// Buffers fed bytes and extracts complete frames. Reused across reads on
/// the same connection: `Vec::drain` shifts the retained tail to the
/// front without reallocating, so a connection that settles into a
/// steady message size stops growing its buffer after the first few
/// reads.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append newly-read bytes, e.g. straight from a `read()` call.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Extract every complete frame currently buffered, calling `f` once
    /// per frame with its raw bytes (tag included). Any trailing partial
    /// frame is retained for the next `feed`.
    ///
    /// An unrecognized tag is unrecoverable — without a known length there
    /// is no way to know how many bytes to skip to find the next frame —
    /// so this stops and returns the error immediately rather than
    /// attempting to resynchronize. Frames already extracted before the
    /// bad tag was reached have already been passed to `f`.
    pub fn drain_frames(&mut self, mut f: impl FnMut(&[u8])) -> Result<(), RejectReason> {
        let mut consumed = 0;
        loop {
            let remaining = &self.buf[consumed..];
            let Some(&tag) = remaining.first() else {
                break;
            };
            let Some(len) = message_len(tag) else {
                self.buf.drain(..consumed);
                return Err(RejectReason::UnknownMessageType);
            };
            if remaining.len() < len {
                break; // trailing partial frame -- wait for more bytes
            }
            f(&remaining[..len]);
            consumed += len;
        }
        self.buf.drain(..consumed);
        Ok(())
    }

    /// Bytes currently buffered but not yet extracted as a complete frame.
    /// For tests and diagnostics.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::encode_command;
    use crate::tag::MAX_MESSAGE_LEN;
    use core::event::Command;
    use core::types::{AccountId, OrderId};

    fn cancel_order_bytes(order: u64) -> Vec<u8> {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_command(
            &Command::CancelOrder {
                account_id: AccountId(1),
                order_id: OrderId(order),
            },
            &mut buf,
        );
        buf[..len].to_vec()
    }

    #[test]
    fn frame_split_across_two_reads() {
        let frame = cancel_order_bytes(1);
        let mid = frame.len() / 2;

        let mut framer = Framer::new();
        let mut extracted: Vec<Vec<u8>> = Vec::new();

        // First read only delivers the first half of the frame.
        framer.feed(&frame[..mid]);
        framer.drain_frames(|f| extracted.push(f.to_vec())).unwrap();
        assert!(extracted.is_empty(), "must not extract a partial frame");
        assert_eq!(framer.pending(), mid);

        // Second read delivers the rest.
        framer.feed(&frame[mid..]);
        framer.drain_frames(|f| extracted.push(f.to_vec())).unwrap();
        assert_eq!(extracted, vec![frame]);
        assert_eq!(framer.pending(), 0);
    }

    #[test]
    fn two_frames_in_one_read() {
        let frame_a = cancel_order_bytes(1);
        let frame_b = cancel_order_bytes(2);
        let mut combined = frame_a.clone();
        combined.extend_from_slice(&frame_b);

        let mut framer = Framer::new();
        let mut extracted: Vec<Vec<u8>> = Vec::new();

        framer.feed(&combined);
        framer.drain_frames(|f| extracted.push(f.to_vec())).unwrap();

        assert_eq!(extracted, vec![frame_a, frame_b]);
        assert_eq!(framer.pending(), 0);
    }

    #[test]
    fn trailing_partial_after_several_complete() {
        let frame_a = cancel_order_bytes(1);
        let frame_b = cancel_order_bytes(2);
        let frame_c = cancel_order_bytes(3);
        let mut combined = frame_a.clone();
        combined.extend_from_slice(&frame_b);
        combined.extend_from_slice(&frame_c);
        // Trailing partial: only the first three bytes of a fourth frame.
        let partial = &frame_c[..3];
        combined.extend_from_slice(partial);

        let mut framer = Framer::new();
        let mut extracted: Vec<Vec<u8>> = Vec::new();

        framer.feed(&combined);
        framer.drain_frames(|f| extracted.push(f.to_vec())).unwrap();

        assert_eq!(extracted, vec![frame_a, frame_b, frame_c.clone()]);
        // The partial fourth frame is retained, not dropped.
        assert_eq!(framer.pending(), 3);

        // Completing it produces exactly the fourth frame.
        framer.feed(&frame_c[3..]);
        extracted.clear();
        framer.drain_frames(|f| extracted.push(f.to_vec())).unwrap();
        assert_eq!(extracted, vec![frame_c]);
        assert_eq!(framer.pending(), 0);
    }

    #[test]
    fn unknown_tag_stops_framing_but_keeps_prior_frames() {
        let frame_a = cancel_order_bytes(1);
        let mut combined = frame_a.clone();
        combined.push(255); // unrecognized tag

        let mut framer = Framer::new();
        let mut extracted: Vec<Vec<u8>> = Vec::new();

        framer.feed(&combined);
        let result = framer.drain_frames(|f| extracted.push(f.to_vec()));

        assert_eq!(result, Err(RejectReason::UnknownMessageType));
        assert_eq!(extracted, vec![frame_a]);
    }

    #[test]
    fn empty_buffer_extracts_nothing() {
        let mut framer = Framer::new();
        let mut extracted: Vec<Vec<u8>> = Vec::new();
        framer.drain_frames(|f| extracted.push(f.to_vec())).unwrap();
        assert!(extracted.is_empty());
    }
}
