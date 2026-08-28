use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Usage,
    Stale,
    ScopeDrift,
    JournalBlocked,
    NotFound,
    Ambiguous,
    Overlap,
    Malformed,
    Toctou,
    Io,
}

impl ErrorKind {
    fn label(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Stale => "stale",
            Self::ScopeDrift => "scope-drift",
            Self::JournalBlocked => "journal-blocked",
            Self::NotFound => "not-found",
            Self::Ambiguous => "ambiguous",
            Self::Overlap => "overlap",
            Self::Malformed => "malformed",
            Self::Toctou => "toctou",
            Self::Io => "io",
        }
    }
}

#[derive(Debug, Clone)]
pub struct VcError {
    pub kind: ErrorKind,
    pub message: String,
    pub next: Option<String>,
}

pub type VcResult<T> = Result<T, VcError>;

impl VcError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            next: None,
        }
    }
    pub fn with_next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }
    pub fn exit_code(&self) -> i32 {
        match self.kind {
            ErrorKind::Usage => 2,
            ErrorKind::Stale => 3,
            ErrorKind::ScopeDrift => 4,
            ErrorKind::JournalBlocked => 5,
            _ => 1,
        }
    }
}

impl fmt::Display for VcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.label(), self.message)?;
        if let Some(n) = &self.next {
            write!(f, " — next: {n}")?;
        }
        Ok(())
    }
}

impl std::error::Error for VcError {}

impl From<std::io::Error> for VcError {
    fn from(e: std::io::Error) -> Self {
        VcError::new(ErrorKind::Io, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_uses_error_grammar() {
        let e = VcError::new(ErrorKind::Stale, "src/config.rs changed since plan")
            .with_next("vc plan --refresh 77bd02aa");
        assert_eq!(
            e.to_string(),
            "stale: src/config.rs changed since plan — next: vc plan --refresh 77bd02aa"
        );
        let e2 = VcError::new(ErrorKind::Io, "permission denied");
        assert_eq!(e2.to_string(), "io: permission denied");
    }
    #[test]
    fn exit_codes_match_spec() {
        assert_eq!(VcError::new(ErrorKind::Usage, "").exit_code(), 2);
        assert_eq!(VcError::new(ErrorKind::Stale, "").exit_code(), 3);
        assert_eq!(VcError::new(ErrorKind::ScopeDrift, "").exit_code(), 4);
        assert_eq!(VcError::new(ErrorKind::JournalBlocked, "").exit_code(), 5);
        for k in [
            ErrorKind::NotFound,
            ErrorKind::Ambiguous,
            ErrorKind::Overlap,
            ErrorKind::Malformed,
            ErrorKind::Toctou,
            ErrorKind::Io,
        ] {
            assert_eq!(VcError::new(k, "").exit_code(), 1);
        }
    }
}
