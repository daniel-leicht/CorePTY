//! SFTP version 3 wire format (draft-ietf-secsh-filexfer-02), the version
//! OpenSSH and practically every server speaks. Paths and names stay raw
//! bytes end to end: a remote file whose name isn't valid UTF-8 can still be
//! listed, opened, renamed and deleted.

use std::io;

pub const VERSION: u32 = 3;

// Packet types.
pub const FXP_INIT: u8 = 1;
pub const FXP_VERSION: u8 = 2;
pub const FXP_OPEN: u8 = 3;
pub const FXP_CLOSE: u8 = 4;
pub const FXP_READ: u8 = 5;
pub const FXP_WRITE: u8 = 6;
pub const FXP_LSTAT: u8 = 7;
pub const FXP_FSTAT: u8 = 8;
pub const FXP_SETSTAT: u8 = 9;
pub const FXP_OPENDIR: u8 = 11;
pub const FXP_READDIR: u8 = 12;
pub const FXP_REMOVE: u8 = 13;
pub const FXP_MKDIR: u8 = 14;
pub const FXP_RMDIR: u8 = 15;
pub const FXP_REALPATH: u8 = 16;
pub const FXP_STAT: u8 = 17;
pub const FXP_RENAME: u8 = 18;
pub const FXP_READLINK: u8 = 19;
pub const FXP_SYMLINK: u8 = 20;
pub const FXP_STATUS: u8 = 101;
pub const FXP_HANDLE: u8 = 102;
pub const FXP_DATA: u8 = 103;
pub const FXP_NAME: u8 = 104;
pub const FXP_ATTRS: u8 = 105;
pub const FXP_EXTENDED: u8 = 200;
pub const FXP_EXTENDED_REPLY: u8 = 201;

// Open flags.
pub const FXF_READ: u32 = 0x01;
pub const FXF_WRITE: u32 = 0x02;
pub const FXF_CREAT: u32 = 0x08;
pub const FXF_TRUNC: u32 = 0x10;
pub const FXF_EXCL: u32 = 0x20;

// Attribute flags.
const ATTR_SIZE: u32 = 0x01;
const ATTR_UIDGID: u32 = 0x02;
const ATTR_PERMISSIONS: u32 = 0x04;
const ATTR_ACMODTIME: u32 = 0x08;
const ATTR_EXTENDED: u32 = 0x8000_0000;

// Status codes. v3 defines 0–8; the rest are later drafts' codes, which some
// v3 servers send anyway.
pub const FX_OK: u32 = 0;
pub const FX_EOF: u32 = 1;
pub const FX_NO_SUCH_FILE: u32 = 2;
pub const FX_PERMISSION_DENIED: u32 = 3;
pub const FX_FAILURE: u32 = 4;
pub const FX_BAD_MESSAGE: u32 = 5;
pub const FX_NO_CONNECTION: u32 = 6;
pub const FX_CONNECTION_LOST: u32 = 7;
pub const FX_OP_UNSUPPORTED: u32 = 8;
const FX_INVALID_HANDLE: u32 = 9;
const FX_NO_SUCH_PATH: u32 = 10;
const FX_FILE_ALREADY_EXISTS: u32 = 11;
const FX_WRITE_PROTECT: u32 = 12;
const FX_NO_MEDIA: u32 = 13;
const FX_NO_SPACE_ON_FILESYSTEM: u32 = 14;
const FX_QUOTA_EXCEEDED: u32 = 15;
const FX_DIR_NOT_EMPTY: u32 = 18;
const FX_NOT_A_DIRECTORY: u32 = 19;
const FX_INVALID_FILENAME: u32 = 20;
const FX_LINK_LOOP: u32 = 21;
const FX_FILE_IS_A_DIRECTORY: u32 = 24;

/// POSIX file type bits inside `permissions`.
pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;

/// Largest packet accepted from the server. OpenSSH sends at most 256 KiB;
/// anything far beyond that is a broken stream, not a real reply.
pub const MAX_PACKET: usize = 4 * 1024 * 1024;

/// File attributes; every field is optional on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Permission bits plus the file type (`S_IFMT`) bits.
    pub permissions: Option<u32>,
    /// Seconds since the Unix epoch. v3 sends both or neither.
    pub atime: Option<u32>,
    pub mtime: Option<u32>,
}

impl Attrs {
    pub fn file_type(&self) -> Option<u32> {
        self.permissions.map(|mode| mode & S_IFMT)
    }

    pub fn is_dir(&self) -> bool {
        self.file_type() == Some(S_IFDIR)
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type() == Some(S_IFLNK)
    }
}

/// One entry of a NAME reply (a directory listing, or a resolved path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameEntry {
    pub filename: Vec<u8>,
    /// `ls -l` style line; the only place v3 carries owner and group names.
    pub longname: Vec<u8>,
    pub attrs: Attrs,
}

/// A reply from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Status { code: u32, message: String },
    Handle(Vec<u8>),
    Data(Vec<u8>),
    Name(Vec<NameEntry>),
    Attrs(Attrs),
    ExtendedReply(Vec<u8>),
}

/// Builds one packet: `uint32 length, byte type, payload`.
pub struct Encoder {
    buffer: Vec<u8>,
}

impl Encoder {
    pub fn new(kind: u8) -> Self {
        let mut buffer = Vec::with_capacity(64);
        buffer.extend_from_slice(&[0, 0, 0, 0, kind]);
        Self { buffer }
    }

    /// Room for `extra` payload bytes up front (writes carry whole chunks).
    pub fn with_capacity(kind: u8, extra: usize) -> Self {
        let mut buffer = Vec::with_capacity(extra + 64);
        buffer.extend_from_slice(&[0, 0, 0, 0, kind]);
        Self { buffer }
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.buffer.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.buffer.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// An SSH `string`: length-prefixed bytes.
    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.u32(value.len() as u32);
        self.buffer.extend_from_slice(value);
        self
    }

    pub fn attrs(&mut self, attrs: &Attrs) -> &mut Self {
        let mut flags = 0;
        if attrs.size.is_some() {
            flags |= ATTR_SIZE;
        }
        if attrs.uid.is_some() && attrs.gid.is_some() {
            flags |= ATTR_UIDGID;
        }
        if attrs.permissions.is_some() {
            flags |= ATTR_PERMISSIONS;
        }
        // v3 sets both times or neither: a lone one is sent for both.
        let times = match (attrs.atime, attrs.mtime) {
            (Some(atime), Some(mtime)) => Some((atime, mtime)),
            (Some(time), None) | (None, Some(time)) => Some((time, time)),
            (None, None) => None,
        };
        if times.is_some() {
            flags |= ATTR_ACMODTIME;
        }
        self.u32(flags);
        if let Some(size) = attrs.size {
            self.u64(size);
        }
        if let (Some(uid), Some(gid)) = (attrs.uid, attrs.gid) {
            self.u32(uid).u32(gid);
        }
        if let Some(permissions) = attrs.permissions {
            self.u32(permissions);
        }
        if let Some((atime, mtime)) = times {
            self.u32(atime).u32(mtime);
        }
        self
    }

    /// The finished packet, with its length filled in.
    pub fn finish(mut self) -> Vec<u8> {
        let length = (self.buffer.len() - 4) as u32;
        self.buffer[..4].copy_from_slice(&length.to_be_bytes());
        self.buffer
    }
}

/// Reads fields from a packet body.
pub struct Decoder<'a> {
    data: &'a [u8],
}

pub fn bad_message(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the SFTP server sent a malformed reply ({what})"),
    )
}

impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if self.data.len() < count {
            return Err(bad_message("truncated packet"));
        }
        let (head, rest) = self.data.split_at(count);
        self.data = rest;
        Ok(head)
    }

    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> io::Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub fn u64(&mut self) -> io::Result<u64> {
        let bytes = self.take(8)?;
        let mut array = [0; 8];
        array.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(array))
    }

    pub fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.data)
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn attrs(&mut self) -> io::Result<Attrs> {
        let flags = self.u32()?;
        let mut attrs = Attrs::default();
        if flags & ATTR_SIZE != 0 {
            attrs.size = Some(self.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            attrs.uid = Some(self.u32()?);
            attrs.gid = Some(self.u32()?);
        }
        if flags & ATTR_PERMISSIONS != 0 {
            attrs.permissions = Some(self.u32()?);
        }
        if flags & ATTR_ACMODTIME != 0 {
            attrs.atime = Some(self.u32()?);
            attrs.mtime = Some(self.u32()?);
        }
        if flags & ATTR_EXTENDED != 0 {
            for _ in 0..self.u32()? {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(attrs)
    }
}

/// Parse a reply body (after the length): its request id and contents.
pub fn parse_response(body: &[u8]) -> io::Result<(u32, Response)> {
    let mut decoder = Decoder::new(body);
    let kind = decoder.u8()?;
    let id = decoder.u32()?;
    let response = match kind {
        FXP_STATUS => {
            let code = decoder.u32()?;
            // The message and language tag are optional in practice.
            let message = if decoder.is_empty() {
                String::new()
            } else {
                String::from_utf8_lossy(decoder.bytes()?).into_owned()
            };
            Response::Status { code, message }
        }
        FXP_HANDLE => Response::Handle(decoder.bytes()?.to_vec()),
        FXP_DATA => Response::Data(decoder.bytes()?.to_vec()),
        FXP_NAME => {
            let count = decoder.u32()?;
            // Don't let a corrupt count pre-allocate gigabytes.
            let mut entries = Vec::with_capacity(count.min(1024) as usize);
            for _ in 0..count {
                entries.push(NameEntry {
                    filename: decoder.bytes()?.to_vec(),
                    longname: decoder.bytes()?.to_vec(),
                    attrs: decoder.attrs()?,
                });
            }
            Response::Name(entries)
        }
        FXP_ATTRS => Response::Attrs(decoder.attrs()?),
        FXP_EXTENDED_REPLY => Response::ExtendedReply(decoder.rest().to_vec()),
        other => return Err(bad_message(&format!("unknown packet type {other}"))),
    };
    Ok((id, response))
}

/// Extension names and data, as the server announced them.
pub type ExtensionList = Vec<(String, Vec<u8>)>;

/// The `VERSION` reply: protocol version and the server's extensions.
pub fn parse_version(body: &[u8]) -> io::Result<(u32, ExtensionList)> {
    let mut decoder = Decoder::new(body);
    if decoder.u8()? != FXP_VERSION {
        return Err(bad_message("expected VERSION"));
    }
    let version = decoder.u32()?;
    let mut extensions = Vec::new();
    while !decoder.is_empty() {
        let name = String::from_utf8_lossy(decoder.bytes()?).into_owned();
        let data = decoder.bytes()?.to_vec();
        extensions.push((name, data));
    }
    Ok((version, extensions))
}

/// A status code as an `io::Error` the rest of the app understands: the
/// right `ErrorKind` (callers react to `NotFound`, `AlreadyExists`…) and a
/// readable message, plus the server's own text when it says more.
pub fn status_error(code: u32, message: &str) -> io::Error {
    use io::ErrorKind as Kind;
    let (kind, text) = match code {
        FX_EOF => (Kind::UnexpectedEof, "unexpected end of file"),
        FX_NO_SUCH_FILE | FX_NO_SUCH_PATH => (Kind::NotFound, "no such file or folder"),
        FX_PERMISSION_DENIED => (Kind::PermissionDenied, "permission denied"),
        FX_FAILURE => (Kind::Other, "the server couldn't do it"),
        FX_BAD_MESSAGE => (Kind::InvalidData, "the server rejected the request"),
        FX_NO_CONNECTION | FX_CONNECTION_LOST => {
            (Kind::ConnectionAborted, "the connection was lost")
        }
        FX_OP_UNSUPPORTED => (Kind::Unsupported, "the server doesn't support this"),
        FX_INVALID_HANDLE => (Kind::InvalidInput, "the file is no longer open"),
        FX_FILE_ALREADY_EXISTS => (Kind::AlreadyExists, "it already exists"),
        FX_WRITE_PROTECT => (Kind::ReadOnlyFilesystem, "the remote disk is read-only"),
        FX_NO_MEDIA => (Kind::NotFound, "no media in the remote drive"),
        FX_NO_SPACE_ON_FILESYSTEM => (Kind::StorageFull, "the remote disk is full"),
        FX_QUOTA_EXCEEDED => (Kind::QuotaExceeded, "your quota on the server is used up"),
        FX_DIR_NOT_EMPTY => (Kind::DirectoryNotEmpty, "the folder isn't empty"),
        FX_NOT_A_DIRECTORY => (Kind::NotADirectory, "not a folder"),
        FX_INVALID_FILENAME => (Kind::InvalidInput, "the server doesn't allow this name"),
        FX_LINK_LOOP => (Kind::Other, "too many levels of symbolic links"),
        FX_FILE_IS_A_DIRECTORY => (Kind::IsADirectory, "it's a folder"),
        _ => (Kind::Other, "the server reported an error"),
    };
    // OpenSSH's messages just restate the code ("No such file", "Failure");
    // anything more specific is worth showing.
    let generic = [
        "",
        "success",
        "end of file",
        "no such file",
        "permission denied",
        "failure",
        "bad message",
        "no connection",
        "connection lost",
        "operation unsupported",
    ];
    let trimmed = message.trim();
    if generic.contains(&trimmed.to_ascii_lowercase().as_str()) {
        io::Error::new(kind, text)
    } else {
        io::Error::new(kind, format!("{text} ({trimmed})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(packet: &[u8]) -> &[u8] {
        let length = u32::from_be_bytes([packet[0], packet[1], packet[2], packet[3]]) as usize;
        assert_eq!(length, packet.len() - 4, "length prefix");
        &packet[4..]
    }

    #[test]
    fn encodes_requests_with_length_and_raw_byte_paths() {
        let mut packet = Encoder::new(FXP_OPEN);
        packet
            .u32(7)
            .bytes(b"/tmp/caf\xe9.txt") // Latin-1, not valid UTF-8
            .u32(FXF_WRITE | FXF_CREAT | FXF_EXCL)
            .attrs(&Attrs::default());
        let packet = packet.finish();
        let mut decoder = Decoder::new(body(&packet));
        assert_eq!(decoder.u8().unwrap(), FXP_OPEN);
        assert_eq!(decoder.u32().unwrap(), 7);
        assert_eq!(decoder.bytes().unwrap(), b"/tmp/caf\xe9.txt");
        assert_eq!(decoder.u32().unwrap(), 0x2a);
        assert_eq!(decoder.attrs().unwrap(), Attrs::default());
        assert!(decoder.is_empty());
    }

    #[test]
    fn attributes_round_trip_and_lone_times_are_sent_as_a_pair() {
        let full = Attrs {
            size: Some(1 << 40),
            uid: Some(1000),
            gid: Some(100),
            permissions: Some(S_IFREG | 0o640),
            atime: Some(1_700_000_000),
            mtime: Some(1_700_000_100),
        };
        let mut packet = Encoder::new(FXP_ATTRS);
        packet.attrs(&full);
        let packet = packet.finish();
        let mut decoder = Decoder::new(&body(&packet)[1..]);
        assert_eq!(decoder.attrs().unwrap(), full);

        let mut packet = Encoder::new(FXP_SETSTAT);
        packet.attrs(&Attrs {
            mtime: Some(42),
            ..Attrs::default()
        });
        let packet = packet.finish();
        let decoded = Decoder::new(&body(&packet)[1..]).attrs().unwrap();
        assert_eq!((decoded.atime, decoded.mtime), (Some(42), Some(42)));
    }

    #[test]
    fn parses_name_status_and_data_replies() {
        let mut name = Encoder::new(FXP_NAME);
        name.u32(3).u32(2);
        for file in [&b"a"[..], b"\xff\xfe"] {
            name.bytes(file)
                .bytes(b"-rw-r--r--  1 me staff 0 Jan 1 00:00 x")
                .attrs(&Attrs {
                    size: Some(5),
                    ..Attrs::default()
                });
        }
        let packet = name.finish();
        let (id, response) = parse_response(body(&packet)).unwrap();
        assert_eq!(id, 3);
        let Response::Name(entries) = response else {
            panic!("expected a name reply");
        };
        assert_eq!(entries[1].filename, b"\xff\xfe");
        assert_eq!(entries[0].attrs.size, Some(5));

        // Some servers omit the message and language tag.
        let mut status = Encoder::new(FXP_STATUS);
        status.u32(9).u32(FX_EOF);
        let packet = status.finish();
        assert_eq!(
            parse_response(body(&packet)).unwrap(),
            (
                9,
                Response::Status {
                    code: FX_EOF,
                    message: String::new()
                }
            )
        );

        let mut data = Encoder::new(FXP_DATA);
        data.u32(1).bytes(b"hello");
        let packet = data.finish();
        assert_eq!(
            parse_response(body(&packet)).unwrap().1,
            Response::Data(b"hello".to_vec())
        );
    }

    #[test]
    fn truncated_or_unknown_packets_are_errors_not_panics() {
        assert!(parse_response(&[FXP_NAME, 0, 0, 0, 1, 0, 0, 0, 5]).is_err());
        assert!(parse_response(&[FXP_DATA, 0, 0]).is_err());
        assert!(parse_response(&[77, 0, 0, 0, 1]).is_err());
        assert!(parse_response(&[]).is_err());
    }

    #[test]
    fn status_codes_map_to_error_kinds_and_keep_specific_messages() {
        let missing = status_error(FX_NO_SUCH_FILE, "No such file");
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert_eq!(missing.to_string(), "no such file or folder");
        let denied = status_error(FX_PERMISSION_DENIED, "Permission denied");
        assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
        let full = status_error(FX_FAILURE, "No space left on device");
        assert_eq!(
            full.to_string(),
            "the server couldn't do it (No space left on device)"
        );
        assert_eq!(
            status_error(FX_FILE_ALREADY_EXISTS, "").kind(),
            io::ErrorKind::AlreadyExists
        );
    }
}
