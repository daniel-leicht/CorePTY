//! Tiny framed protocol spoken over the named pipe that connects the
//! (non-elevated) app to the elevated broker. Both ends are the same binary, so
//! they always agree on the wire format.
//!
//! Frame = `[tag: u8][len: u32 LE][payload: len bytes]`.

use std::io::{self, Read, Write};

/// Raw terminal bytes — app→broker is keystrokes, broker→app is output.
pub const TAG_DATA: u8 = 1;
/// app→broker resize. Payload = `[cols: u16 LE][rows: u16 LE]`.
pub const TAG_RESIZE: u8 = 2;
/// broker→app shell exit. Payload = `[code: i32 LE]`.
pub const TAG_EXIT: u8 = 3;
/// app→broker: stop the elevated shell now (the tab was closed).
pub const TAG_CLOSE: u8 = 4;

/// Largest payload we'll accept in one frame. PTY chunks are ≤64 KiB; this cap
/// guards against a corrupt or hostile length prefix triggering a huge (up to
/// 4 GiB) allocation before any bytes are even sent.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct Frame {
    pub tag: u8,
    pub payload: Vec<u8>,
}

pub fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 5];
    header[0] = tag;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&header)?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<Frame> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame length exceeds maximum",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Frame {
        tag: header[0],
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_round_trip_preserves_binary_payload() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, TAG_DATA, &[0, 1, 255]).unwrap();
        let frame = read_frame(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(frame.tag, TAG_DATA);
        assert_eq!(frame.payload, [0, 1, 255]);
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        let mut header = vec![TAG_DATA];
        header.extend_from_slice(&((MAX_FRAME_LEN as u32) + 1).to_le_bytes());
        let error = read_frame(&mut Cursor::new(header)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
