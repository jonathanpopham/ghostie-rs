//! Crate-wide error type. One enum, `std::error::Error + Display`, with
//! actionable messages (file path + line where relevant). No external error
//! crates — zero dependencies is law.

use std::fmt;

/// Every fallible ghostie operation returns this error.
///
/// Messages are written for the person (or agent) reading them: they name the
/// file, the field, and what was expected, so a failure is diagnosable from
/// its output alone.
#[derive(Debug)]
pub enum Error {
    /// An I/O failure, with the path and operation that failed.
    Io {
        /// What ghostie was doing, e.g. "writing memory file".
        context: String,
        /// The path involved.
        path: String,
        /// The underlying OS error.
        source: std::io::Error,
    },
    /// A timestamp string that is not the strict RFC3339 UTC form
    /// `YYYY-MM-DDTHH:MM:SSZ` this crate uses.
    InvalidTimestamp {
        /// The offending value.
        value: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// A malformed document (JSON, frontmatter, ...) with location info.
    Parse {
        /// Where the document came from, e.g. a file path or "<stdin>".
        origin: String,
        /// 1-based line number when known, 0 when not.
        line: usize,
        /// What went wrong, in plain words.
        message: String,
    },
    /// A well-formed document whose content violates the memory schema.
    Invalid {
        /// Where the document came from.
        origin: String,
        /// What is invalid and what was expected.
        message: String,
    },
    /// Command-line usage error (maps to exit code 2).
    Usage {
        /// What was wrong with the invocation.
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io {
                context,
                path,
                source,
            } => {
                write!(f, "{context}: {path}: {source}")
            }
            Error::InvalidTimestamp { value, reason } => {
                write!(
                    f,
                    "invalid timestamp {value:?}: {reason} (expected RFC3339 UTC, e.g. 2026-07-13T12:00:00Z)"
                )
            }
            Error::Parse {
                origin,
                line,
                message,
            } => {
                if *line == 0 {
                    write!(f, "{origin}: {message}")
                } else {
                    write!(f, "{origin}:{line}: {message}")
                }
            }
            Error::Invalid { origin, message } => write!(f, "{origin}: {message}"),
            Error::Usage { message } => write!(f, "usage error: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// A structured, non-fatal problem: surfaced to humans on stderr and to
/// robots inside the `--json` envelope's `warnings` array. Warnings never
/// stop an operation — one bad file must not take the store down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Where the problem lives (usually a file path).
    pub origin: String,
    /// What is wrong and what was expected, in plain words.
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.origin, self.message)
    }
}
