//! Transport records (docs/BRIDGE-PROTOCOL.md "Transport records"): every Noise
//! message's plaintext is `[kind u8][flags u8][body…]`. Kind 0 carries chunks of
//! a JSON frame, kind 1 carries media bytes as `[u32 streamId][bytes]`.

/// A Noise message is at most 65,535 bytes, 16 of them the tag.
pub const MAX_NOISE_MESSAGE: usize = 65_535;
/// Body bytes per record: 65,535 − 16-byte tag − 2 header bytes.
pub const MAX_BODY: usize = MAX_NOISE_MESSAGE - 16 - 2;
/// Media bytes per stream record (the body also holds the stream id).
pub const MAX_STREAM_CHUNK: usize = MAX_BODY - 4;

pub const KIND_JSON: u8 = 0;
pub const KIND_STREAM: u8 = 1;
const MORE: u8 = 1;
const END: u8 = 1;
const ABORT: u8 = 2;

#[derive(Debug, PartialEq)]
pub enum Record<'a> {
    Json { chunk: &'a [u8], more: bool },
    Stream { id: u32, bytes: &'a [u8], end: bool, aborted: bool },
}

/// The plaintexts that carry one JSON frame.
pub fn json_records(frame: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = if frame.is_empty() { vec![&[]] } else { frame.chunks(MAX_BODY).collect() };
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut out = Vec::with_capacity(2 + chunk.len());
            out.push(KIND_JSON);
            out.push(if i < last { MORE } else { 0 });
            out.extend_from_slice(chunk);
            out
        })
        .collect()
}

/// One stream record. `bytes` must fit in [`MAX_STREAM_CHUNK`].
pub fn stream_record(id: u32, bytes: &[u8], end: bool, aborted: bool) -> Vec<u8> {
    debug_assert!(bytes.len() <= MAX_STREAM_CHUNK);
    let mut out = Vec::with_capacity(6 + bytes.len());
    out.push(KIND_STREAM);
    out.push(if end { END } else { 0 } | if aborted { ABORT } else { 0 });
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

pub fn parse(plain: &[u8]) -> Result<Record<'_>, &'static str> {
    let (&kind, rest) = plain.split_first().ok_or("empty record")?;
    let (&flags, body) = rest.split_first().ok_or("record without flags")?;
    match kind {
        KIND_JSON => Ok(Record::Json { chunk: body, more: flags & MORE != 0 }),
        KIND_STREAM => {
            if body.len() < 4 {
                return Err("stream record without an id");
            }
            let id = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
            Ok(Record::Stream { id, bytes: &body[4..], end: flags & END != 0, aborted: flags & ABORT != 0 })
        }
        _ => Err("unknown record kind"),
    }
}

/// Joins JSON chunks back into frames, refusing frames over `max` bytes.
pub struct Joiner {
    buf: Vec<u8>,
    max: usize,
}

impl Joiner {
    pub fn new(max: usize) -> Self {
        Self { buf: Vec::new(), max }
    }

    /// The whole frame once its last chunk arrives.
    pub fn push(&mut self, chunk: &[u8], more: bool) -> Result<Option<Vec<u8>>, &'static str> {
        if self.buf.len() + chunk.len() > self.max {
            return Err("frame too large");
        }
        self.buf.extend_from_slice(chunk);
        Ok((!more).then(|| std::mem::take(&mut self.buf)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_frames_are_one_record() {
        let records = json_records(br#"{"a":1}"#);
        assert_eq!(records, vec![[&[0u8, 0][..], br#"{"a":1}"#].concat()]);
    }

    #[test]
    fn big_frames_split_and_join() {
        let frame: Vec<u8> = (0..(MAX_BODY * 2 + 10)).map(|i| (i % 251) as u8).collect();
        let records = json_records(&frame);
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|r| r.len() <= MAX_NOISE_MESSAGE - 16));
        let mut joiner = Joiner::new(1 << 20);
        let mut out = None;
        for r in &records {
            let Record::Json { chunk, more } = parse(r).unwrap() else { panic!() };
            out = joiner.push(chunk, more).unwrap();
        }
        assert_eq!(out.unwrap(), frame);
    }

    #[test]
    fn joiner_caps_frames() {
        let mut joiner = Joiner::new(10);
        assert!(joiner.push(&[0; 8], true).unwrap().is_none());
        assert!(joiner.push(&[0; 8], false).is_err());
    }

    #[test]
    fn stream_records_round_trip() {
        let r = stream_record(7, b"abc", true, false);
        assert_eq!(parse(&r).unwrap(), Record::Stream { id: 7, bytes: b"abc", end: true, aborted: false });
        let r = stream_record(9, b"why", true, true);
        assert_eq!(parse(&r).unwrap(), Record::Stream { id: 9, bytes: b"why", end: true, aborted: true });
        assert!(parse(&[5, 0]).is_err());
    }
}
