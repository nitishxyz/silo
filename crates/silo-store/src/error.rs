use std::fmt;
use std::io;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// Envelope is malformed or truncated (bad magic, too short).
    Corrupt,
    /// Authentication failed: wrong key, wrong AAD or tampered contents.
    AuthFailed,
    /// Key file problem (symlink, bad permissions, wrong length, ...).
    InsecureKey {
        path: PathBuf,
        reason: String,
    },
    /// Manifest / payload (de)serialization failure.
    Serde(serde_json::Error),
    /// Invalid snapshot id.
    InvalidId(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Corrupt => write!(f, "sealed data is malformed or truncated"),
            Error::AuthFailed => write!(
                f,
                "sealed data failed authentication (wrong key or tampered)"
            ),
            Error::InsecureKey { path, reason } => {
                write!(f, "refusing key file {}: {reason}", path.display())
            }
            Error::Serde(e) => write!(f, "serialization error: {e}"),
            Error::InvalidId(id) => write!(f, "invalid snapshot id {id:?}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Serde(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Serde(e)
    }
}
