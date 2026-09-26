//! Where the message body comes from.
//!
//! The named first consumer of `lnmsg send` is a health monitor running from
//! cron (`docs/src/concepts/lnmsg-architecture.md:626`), and the shape it uses
//! is `echo "disk 91%" | lnmsg send <addr>`. So stdin is the default source,
//! and a positional body is the convenience, not the other way round.

use std::io::Read;

/// Why there is no body to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyError {
    /// stdin (or the positional argument) held nothing but whitespace.
    Empty,
    /// stdin could not be read.
    Unreadable(String),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(
                f,
                "the message body is empty; give it as an argument or on stdin"
            ),
            Self::Unreadable(detail) => write!(f, "could not read the body from stdin: {detail}"),
        }
    }
}

impl std::error::Error for BodyError {}

/// Resolve the body from the positional argument or from `stdin`.
///
/// `None` and `Some("-")` both mean stdin, which is the documented form. A
/// body is bytes, not text: whatever arrives is forwarded byte for byte, so a
/// non-UTF-8 payload is not mangled on its way to a peer that can read it.
///
/// One trailing newline is removed (with a preceding carriage return, if any).
/// Every line-oriented producer appends one — `echo`, `printf '%s\n'`, a
/// here-string — and carrying it into the message would put a blank line at
/// the end of every status line a cron job ever sends. Exactly one is removed,
/// so a body that deliberately ends in a blank line can still say so with two.
pub fn resolve(positional: Option<&str>, stdin: &mut impl Read) -> Result<Vec<u8>, BodyError> {
    let raw = match positional {
        None | Some("-") => {
            let mut buffer = Vec::new();
            stdin
                .read_to_end(&mut buffer)
                .map_err(|e| BodyError::Unreadable(e.to_string()))?;
            buffer
        }
        Some(text) => text.as_bytes().to_vec(),
    };
    let trimmed = strip_one_trailing_newline(raw);
    if trimmed.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(BodyError::Empty);
    }
    Ok(trimmed)
}

fn strip_one_trailing_newline(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_stdin(input: &[u8]) -> Result<Vec<u8>, BodyError> {
        resolve(None, &mut std::io::Cursor::new(input.to_vec()))
    }

    /// The cron shape from the architecture record, verbatim.
    #[test]
    fn echo_into_stdin_loses_its_newline_and_nothing_else() {
        assert_eq!(from_stdin(b"disk 91%\n"), Ok(b"disk 91%".to_vec()));
    }

    #[test]
    fn a_dash_means_stdin_too() {
        let body = resolve(
            Some("-"),
            &mut std::io::Cursor::new(b"from stdin\n".to_vec()),
        );
        assert_eq!(body, Ok(b"from stdin".to_vec()));
    }

    #[test]
    fn a_positional_body_is_taken_verbatim() {
        let body = resolve(Some("hello there"), &mut std::io::Cursor::new(Vec::new()));
        assert_eq!(body, Ok(b"hello there".to_vec()));
    }

    #[test]
    fn only_one_trailing_newline_goes() {
        assert_eq!(from_stdin(b"two\n\n"), Ok(b"two\n".to_vec()));
        assert_eq!(from_stdin(b"crlf\r\n"), Ok(b"crlf".to_vec()));
    }

    #[test]
    fn interior_newlines_survive() {
        assert_eq!(from_stdin(b"one\ntwo\n"), Ok(b"one\ntwo".to_vec()));
    }

    #[test]
    fn an_empty_stdin_is_an_empty_body() {
        assert_eq!(from_stdin(b""), Err(BodyError::Empty));
        assert_eq!(from_stdin(b"\n"), Err(BodyError::Empty));
        assert_eq!(from_stdin(b"   \t\n"), Err(BodyError::Empty));
    }

    /// A body is bytes. Nothing here may assume UTF-8, because the peer that
    /// reads it may not be a terminal at all.
    #[test]
    fn non_utf8_bytes_pass_through_unchanged() {
        assert_eq!(
            from_stdin(b"\xff\xfe\x00ok\n"),
            Ok(b"\xff\xfe\x00ok".to_vec())
        );
    }

    #[test]
    fn a_read_error_is_reported_rather_than_treated_as_empty() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("no stdin here"))
            }
        }
        assert!(matches!(
            resolve(None, &mut Broken),
            Err(BodyError::Unreadable(_))
        ));
    }
}
