//! Provider-native paths and names.
//!
//! Local paths keep their OS encoding (UTF-16 on Windows, bytes on Unix), so no
//! file name is ever mangled on its way through the UI. Remote (SFTP) paths are
//! raw POSIX bytes. Both cross IPC as opaque tokens beside a lossy display
//! string; the frontend never builds or edits a path itself.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    io,
    path::{Component, Path, PathBuf},
};

/// A path on some filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VPath {
    /// A path on this machine, in the OS's own encoding.
    Native(PathBuf),
    /// A POSIX path on a remote host, as raw bytes (`/home/me`).
    Posix(Vec<u8>),
}

/// One path component (a file or folder name) of either flavor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VName {
    Native(OsString),
    Posix(Vec<u8>),
}

impl VPath {
    #[allow(dead_code)] // Used by tests now and by the SFTP provider (Phase 6).
    pub fn native(path: impl Into<PathBuf>) -> Self {
        Self::Native(path.into())
    }

    #[allow(dead_code)] // Used by tests now and by the SFTP provider (Phase 6).
    pub fn posix(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Posix(bytes.into())
    }

    /// The native path, or an error for a remote path handed to a local provider.
    pub fn to_native(&self) -> io::Result<&Path> {
        match self {
            Self::Native(path) => Ok(path),
            Self::Posix(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a remote path was given to the local filesystem",
            )),
        }
    }

    #[allow(dead_code)] // Used by tests now and by the SFTP provider (Phase 6).
    pub fn to_posix(&self) -> io::Result<&[u8]> {
        match self {
            Self::Posix(bytes) => Ok(bytes),
            Self::Native(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a local path was given to a remote filesystem",
            )),
        }
    }

    pub fn file_name(&self) -> Option<VName> {
        match self {
            Self::Native(path) => path.file_name().map(|name| VName::Native(name.to_owned())),
            Self::Posix(bytes) => posix_file_name(bytes).map(|name| VName::Posix(name.to_vec())),
        }
    }

    pub fn parent(&self) -> Option<VPath> {
        match self {
            Self::Native(path) => path.parent().map(|parent| Self::Native(parent.to_owned())),
            Self::Posix(bytes) => posix_parent(bytes).map(|parent| Self::Posix(parent.to_vec())),
        }
    }

    /// Append one name, converted to this path's flavor. Fails unless the name
    /// is a single plain component here: a remote file called `..\x` must not
    /// climb out of the target folder once it lands on Windows.
    pub fn join(&self, name: &VName) -> io::Result<VPath> {
        match self {
            Self::Native(path) => {
                let name = name.to_native();
                let mut components = Path::new(&*name).components();
                let single = matches!(components.next(), Some(Component::Normal(_)))
                    && components.next().is_none();
                if !single || has_trailing_separator(&name) {
                    return Err(invalid_name(&name.to_string_lossy()));
                }
                Ok(Self::Native(path.join(&*name)))
            }
            Self::Posix(bytes) => {
                let name = name.to_posix();
                if name.is_empty()
                    || &*name == b"."
                    || &*name == b".."
                    || name.contains(&b'/')
                    || name.contains(&0)
                {
                    return Err(invalid_name(&String::from_utf8_lossy(&name)));
                }
                let mut joined = bytes.clone();
                if !joined.is_empty() && !joined.ends_with(b"/") {
                    joined.push(b'/');
                }
                joined.extend_from_slice(&name);
                Ok(Self::Posix(joined))
            }
        }
    }

    /// Whether `self` is `base` or lies inside it (whole components only).
    pub fn starts_with(&self, base: &VPath) -> bool {
        match (self, base) {
            (Self::Native(path), Self::Native(base)) => path.starts_with(base),
            (Self::Posix(path), Self::Posix(base)) => {
                let mut path = posix_components(path);
                posix_components(base).all(|part| path.next() == Some(part))
            }
            _ => false,
        }
    }

    /// Human-readable form; lossy for names that aren't valid Unicode.
    pub fn display(&self) -> String {
        match self {
            Self::Native(path) => path.to_string_lossy().into_owned(),
            Self::Posix(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        }
    }

    pub fn to_token(&self) -> String {
        match self {
            Self::Native(path) => format!("{NATIVE_PREFIX}{}", encode_native(path)),
            Self::Posix(bytes) => format!("p:{}", URL_SAFE_NO_PAD.encode(bytes)),
        }
    }

    pub fn from_token(token: &str) -> Result<Self, String> {
        let invalid = || "invalid path token".to_string();
        if let Some(encoded) = token.strip_prefix(NATIVE_PREFIX) {
            return decode_native(encoded).ok_or_else(invalid).map(Self::Native);
        }
        if let Some(encoded) = token.strip_prefix("p:") {
            return URL_SAFE_NO_PAD
                .decode(encoded)
                .map(Self::Posix)
                .map_err(|_| invalid());
        }
        Err(invalid())
    }
}

impl Serialize for VPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_token())
    }
}

impl<'de> Deserialize<'de> for VPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let token = String::deserialize(deserializer)?;
        Self::from_token(&token).map_err(de::Error::custom)
    }
}

impl VName {
    /// A name typed by the user, in the flavor of the folder it will go into.
    pub fn for_path(path: &VPath, name: &str) -> Self {
        match path {
            VPath::Native(_) => Self::Native(OsString::from(name)),
            VPath::Posix(_) => Self::Posix(name.as_bytes().to_vec()),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Native(name) => name.to_string_lossy().into_owned(),
            Self::Posix(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        }
    }

    /// This name as a native file name. Remote names are decoded as UTF-8
    /// (lossy) on Windows and passed through untouched on Unix.
    pub fn to_native(&self) -> Cow<'_, OsStr> {
        match self {
            Self::Native(name) => Cow::Borrowed(name),
            Self::Posix(bytes) => posix_bytes_to_native(bytes),
        }
    }

    /// This name as POSIX bytes (UTF-8 for names coming from Windows).
    pub fn to_posix(&self) -> Cow<'_, [u8]> {
        match self {
            Self::Posix(bytes) => Cow::Borrowed(bytes),
            Self::Native(name) => native_to_posix_bytes(name),
        }
    }

    /// The name to store on this machine when it can't hold this one as is,
    /// or `None` when it can. Only Windows forbids names a remote host allows
    /// (`a:b`, `CON`, `notes.`); there the forbidden characters become `_`, a
    /// trailing dot or space gets a `_` after it (Windows would silently drop
    /// it), and device names get a `_` after their stem (writing to `NUL`
    /// would quietly discard the file). A remote name that isn't UTF-8 gets
    /// its readable spelling (`caf�`), so it counts as changed too.
    pub fn storable_here(&self) -> Option<Self> {
        if !cfg!(windows) {
            return None;
        }
        let readable = self.to_native().to_string_lossy().into_owned();
        let undecodable = matches!(self, Self::Posix(bytes) if std::str::from_utf8(bytes).is_err());
        match windows_safe_name(&readable) {
            Some(safe) => Some(Self::Native(OsString::from(safe))),
            None if undecodable => Some(Self::Native(OsString::from(readable))),
            None => None,
        }
    }

    /// This name with `suffix` appended, byte-exact (`movie.mkv.filepart`).
    pub fn with_suffix(&self, suffix: &str) -> Self {
        match self {
            Self::Native(name) => {
                let mut name = name.clone();
                name.push(suffix);
                Self::Native(name)
            }
            Self::Posix(bytes) => {
                let mut bytes = bytes.clone();
                bytes.extend_from_slice(suffix.as_bytes());
                Self::Posix(bytes)
            }
        }
    }

    /// `report.txt` → `report (copy).txt`, then `report (copy 2).txt`, …
    pub fn with_copy_suffix(&self, number: u32) -> Self {
        let suffix = if number <= 1 {
            " (copy)".to_string()
        } else {
            format!(" (copy {number})")
        };
        let display = self.display();
        let lossless = match self {
            Self::Native(name) => name.to_str().is_some(),
            Self::Posix(bytes) => std::str::from_utf8(bytes).is_ok(),
        };
        if !lossless {
            // Keep names that aren't valid Unicode byte-exact: append after the
            // extension rather than round-tripping them through a lossy string.
            return match self {
                Self::Native(name) => {
                    let mut name = name.clone();
                    name.push(&suffix);
                    Self::Native(name)
                }
                Self::Posix(bytes) => {
                    let mut bytes = bytes.clone();
                    bytes.extend_from_slice(suffix.as_bytes());
                    Self::Posix(bytes)
                }
            };
        }
        // A leading dot marks a hidden file (`.bashrc`), not an extension.
        let renamed = match display.rfind('.') {
            Some(dot) if dot > 0 => format!("{}{suffix}{}", &display[..dot], &display[dot..]),
            _ => format!("{display}{suffix}"),
        };
        match self {
            Self::Native(_) => Self::Native(OsString::from(renamed)),
            Self::Posix(_) => Self::Posix(renamed.into_bytes()),
        }
    }
}

/// See [`VName::storable_here`]; `None` when `name` is fine on Windows.
fn windows_safe_name(name: &str) -> Option<String> {
    let mut safe: String = name
        .chars()
        .map(|c| {
            if c < ' ' || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    if safe.ends_with('.') || safe.ends_with(' ') {
        safe.push('_');
    }
    // `CON`, `con.txt`, `COM1 .log`: the part before the first dot names a device.
    let stem_end = safe.find('.').unwrap_or(safe.len());
    let stem = safe[..stem_end].trim_end_matches(' ').to_ascii_uppercase();
    // COM1–COM9 and LPT1–LPT9, and their superscript ¹ ² ³ spellings.
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|number| {
            let mut chars = number.chars();
            matches!(
                (chars.next(), chars.next()),
                (Some('1'..='9' | '¹' | '²' | '³'), None)
            )
        })
    };
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT")
    {
        safe.insert(stem_end, '_');
    }
    (safe != name).then_some(safe)
}

fn invalid_name(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{name:?} is not a valid file name here"),
    )
}

fn has_trailing_separator(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.ends_with('/') || (cfg!(windows) && name.ends_with('\\'))
}

fn posix_components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty() && *part != b".")
}

fn trim_trailing_slashes(path: &[u8]) -> &[u8] {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    &path[..end]
}

fn posix_file_name(path: &[u8]) -> Option<&[u8]> {
    let trimmed = trim_trailing_slashes(path);
    let start = trimmed
        .iter()
        .rposition(|byte| *byte == b'/')
        .map_or(0, |index| index + 1);
    let name = &trimmed[start..];
    if name.is_empty() || name == b"." || name == b".." {
        None
    } else {
        Some(name)
    }
}

fn posix_parent(path: &[u8]) -> Option<&[u8]> {
    let trimmed = trim_trailing_slashes(path);
    if trimmed == b"/" {
        return None;
    }
    match trimmed.iter().rposition(|byte| *byte == b'/') {
        Some(0) => Some(b"/"),
        Some(index) => Some(trim_trailing_slashes(&trimmed[..index])),
        None => None,
    }
}

#[cfg(unix)]
const NATIVE_PREFIX: &str = "u:";
#[cfg(windows)]
const NATIVE_PREFIX: &str = "w:";
#[cfg(not(any(unix, windows)))]
const NATIVE_PREFIX: &str = "s:";

#[cfg(unix)]
fn encode_native(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    URL_SAFE_NO_PAD.encode(path.as_os_str().as_bytes())
}

#[cfg(unix)]
fn decode_native(encoded: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(windows)]
fn encode_native(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let bytes: Vec<u8> = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(windows)]
fn decode_native(encoded: &str) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Some(PathBuf::from(OsString::from_wide(&wide)))
}

#[cfg(not(any(unix, windows)))]
fn encode_native(path: &Path) -> String {
    URL_SAFE_NO_PAD.encode(path.to_string_lossy().as_bytes())
}

#[cfg(not(any(unix, windows)))]
fn decode_native(encoded: &str) -> Option<PathBuf> {
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

#[cfg(unix)]
fn posix_bytes_to_native(bytes: &[u8]) -> Cow<'_, OsStr> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn posix_bytes_to_native(bytes: &[u8]) -> Cow<'_, OsStr> {
    Cow::Owned(OsString::from(String::from_utf8_lossy(bytes).into_owned()))
}

#[cfg(unix)]
fn native_to_posix_bytes(name: &OsStr) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(name.as_bytes())
}

#[cfg(not(unix))]
fn native_to_posix_bytes(name: &OsStr) -> Cow<'_, [u8]> {
    Cow::Owned(name.to_string_lossy().into_owned().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posix(path: &str) -> VPath {
        VPath::posix(path.as_bytes())
    }

    #[test]
    fn names_windows_cannot_store_get_safe_ones() {
        let safe = |name: &str| windows_safe_name(name);
        assert_eq!(safe("report.txt"), None);
        assert_eq!(safe("a:b?.txt").as_deref(), Some("a_b_.txt"));
        assert_eq!(safe("back\\slash").as_deref(), Some("back_slash"));
        assert_eq!(safe("tab\there").as_deref(), Some("tab_here"));
        assert_eq!(safe("notes.").as_deref(), Some("notes._"));
        assert_eq!(safe("trailing ").as_deref(), Some("trailing _"));
        assert_eq!(safe("CON").as_deref(), Some("CON_"));
        assert_eq!(safe("nul.txt").as_deref(), Some("nul_.txt"));
        assert_eq!(safe("com1.log").as_deref(), Some("com1_.log"));
        assert_eq!(safe("COM10"), None);
        assert_eq!(safe("COM0"), None);
        assert_eq!(safe("lpt².txt").as_deref(), Some("lpt²_.txt"));
        assert_eq!(safe("CONOUT$").as_deref(), Some("CONOUT$_"));
        assert_eq!(safe("conin$.log").as_deref(), Some("conin$_.log"));
        assert_eq!(safe("COM⁴"), None);
        assert_eq!(safe("console"), None);
        assert_eq!(safe(".bashrc"), None);
    }

    #[test]
    #[cfg(windows)]
    fn remote_names_that_are_not_utf8_count_as_mapped() {
        let lossy = VName::Posix(b"caf\xe9".to_vec()).storable_here();
        assert_eq!(lossy, Some(VName::Native("caf\u{fffd}".into())));
        assert_eq!(VName::Posix(b"cafe".to_vec()).storable_here(), None);
    }

    #[test]
    fn tokens_round_trip_both_flavors() {
        let native = VPath::native(PathBuf::from("folder").join("hello 🌍.txt"));
        assert_eq!(VPath::from_token(&native.to_token()).unwrap(), native);
        let remote = VPath::posix(b"/srv/\xffraw".to_vec());
        assert_eq!(VPath::from_token(&remote.to_token()).unwrap(), remote);
        assert!(VPath::from_token("local:w:AAAA").is_err());
        assert!(VPath::from_token("p:!!").is_err());
    }

    #[test]
    fn posix_paths_split_and_join() {
        assert_eq!(
            posix("/home/me/notes.txt").file_name(),
            Some(VName::Posix(b"notes.txt".to_vec()))
        );
        assert_eq!(
            posix("/home/me/").file_name(),
            Some(VName::Posix(b"me".to_vec()))
        );
        assert_eq!(posix("/").file_name(), None);
        assert_eq!(posix("/home/me").parent(), Some(posix("/home")));
        assert_eq!(posix("/home").parent(), Some(posix("/")));
        assert_eq!(posix("/").parent(), None);
        let name = VName::Posix(b"a b".to_vec());
        assert_eq!(posix("/srv").join(&name).unwrap(), posix("/srv/a b"));
        assert_eq!(posix("/").join(&name).unwrap(), posix("/a b"));
    }

    #[test]
    fn joins_reject_names_that_are_not_one_component() {
        for bad in ["", ".", "..", "a/b"] {
            assert!(
                posix("/srv").join(&VName::Posix(bad.into())).is_err(),
                "{bad:?}"
            );
            assert!(
                VPath::native("dir")
                    .join(&VName::Native(bad.into()))
                    .is_err(),
                "{bad:?}"
            );
        }
        // A remote name with a backslash would be a path on Windows.
        let sneaky = VName::Posix(b"..\\escape".to_vec());
        assert_eq!(VPath::native("dir").join(&sneaky).is_err(), cfg!(windows));
    }

    #[test]
    fn starts_with_compares_whole_components() {
        assert!(posix("/srv/app/logs").starts_with(&posix("/srv/app")));
        assert!(posix("/srv/app").starts_with(&posix("/srv/app/")));
        assert!(!posix("/srv/application").starts_with(&posix("/srv/app")));
        assert!(!posix("/srv").starts_with(&VPath::native("/srv")));
    }

    #[test]
    fn copy_suffix_keeps_the_extension_and_hidden_dot() {
        let name = VName::Native("report.txt".into());
        assert_eq!(name.with_copy_suffix(1).display(), "report (copy).txt");
        assert_eq!(name.with_copy_suffix(2).display(), "report (copy 2).txt");
        assert_eq!(
            VName::Posix(b".bashrc".to_vec())
                .with_copy_suffix(1)
                .display(),
            ".bashrc (copy)"
        );
        assert_eq!(
            VName::Posix(b"Makefile".to_vec())
                .with_copy_suffix(3)
                .display(),
            "Makefile (copy 3)"
        );
    }
}
