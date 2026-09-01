pub mod color;
pub mod constants;
pub mod palette;
pub mod publish;
pub mod saliency;
pub mod search;
pub mod syntax;
pub mod theme;
pub mod zed_settings;

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    InvalidInput,
    Infeasible,
    External,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    pub fn infeasible(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Infeasible,
            message: message.into(),
        }
    }

    pub fn external(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::External,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn is_infeasible(&self) -> bool {
        self.kind == ErrorKind::Infeasible
    }

    pub fn context(self, context: impl fmt::Display) -> Self {
        Self {
            kind: self.kind,
            message: format!("{context}: {}", self.message),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::external(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::invalid(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
