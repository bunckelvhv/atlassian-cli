//! Encoding for single request path segments.
//!
//! Commands build request paths with `format!`. A caller-provided identifier
//! (an issue key, a repository slug, a team name) that is interpolated as-is can
//! change the request target: a `?` moves the rest of the path into the query,
//! a `#` truncates it, and a `/` adds segments. Every such value goes through
//! [`encode_path_segment`] first.
//!
//! Query values are different and use [`crate::query::encode_query_component`].

use crate::error::{ApiError, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

/// Everything except ASCII letters, digits and `-`, `.`, `_`, `~` is encoded
/// (the RFC 3986 unreserved set). That includes `?`, `#`, `%` and space.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encodes `value` for use as exactly one path segment.
///
/// Values that cannot be a single, literal segment are rejected instead of
/// encoded:
///
/// - an empty value, which would address the collection;
/// - a value containing `/`, which would address a different resource than the
///   one named;
/// - `.` and `..`, which the URL parser resolves as dot segments.
///
/// `%` is encoded, so an identifier that already looks percent-encoded is sent
/// literally and is not decoded a second time by the server.
pub fn encode_path_segment(value: &str) -> Result<String> {
    if value.is_empty() {
        return Err(invalid(value, "must not be empty"));
    }
    if value.contains('/') {
        return Err(invalid(
            value,
            "'/' would address a different resource than the one named",
        ));
    }
    if value == "." || value == ".." {
        return Err(invalid(
            value,
            "a dot component would change which resource the request addresses",
        ));
    }
    Ok(utf8_percent_encode(value, PATH_SEGMENT).to_string())
}

fn invalid(value: &str, reason: &str) -> ApiError {
    ApiError::InvalidIdentifier {
        message: format!("{value:?}: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::encode_path_segment as enc;

    #[test]
    fn unreserved_characters_pass_through() {
        assert_eq!(enc("my-repo.v2_x~1").unwrap(), "my-repo.v2_x~1");
        assert_eq!(enc("PROJ-123").unwrap(), "PROJ-123");
        assert_eq!(enc("..hidden").unwrap(), "..hidden");
        assert_eq!(enc("...").unwrap(), "...");
    }

    #[test]
    fn delimiters_are_encoded() {
        assert_eq!(enc("r?x").unwrap(), "r%3Fx");
        assert_eq!(enc("r#x").unwrap(), "r%23x");
        assert_eq!(enc("r%x").unwrap(), "r%25x");
        assert_eq!(enc("a b").unwrap(), "a%20b");
        assert_eq!(enc("{abc-123}").unwrap(), "%7Babc-123%7D");
        assert_eq!(enc("a\\b").unwrap(), "a%5Cb");
        assert_eq!(enc("a\tb").unwrap(), "a%09b");
    }

    #[test]
    fn a_percent_escape_is_not_decoded_twice() {
        assert_eq!(enc("%2e%2e").unwrap(), "%252e%252e");
        assert_eq!(enc("foo%23").unwrap(), "foo%2523");
    }

    #[test]
    fn non_ascii_is_utf8_percent_encoded() {
        assert_eq!(enc("função").unwrap(), "fun%C3%A7%C3%A3o");
    }

    #[test]
    fn a_slash_is_rejected() {
        assert!(enc("a/b").is_err());
        assert!(enc("/").is_err());
        assert!(enc("myrepo/refs/branches/main").is_err());
    }

    #[test]
    fn dot_segments_and_empty_values_are_rejected() {
        assert!(enc("..").is_err());
        assert!(enc(".").is_err());
        assert!(enc("").is_err());
    }

    #[test]
    fn the_error_names_the_rejected_value() {
        let message = enc("a/b").unwrap_err().to_string();
        assert!(message.contains("\"a/b\""), "{message}");
    }

    /// The consequence, not just the string transform: after encoding, the
    /// resolved URL keeps the identifier inside its own segment.
    #[test]
    fn an_encoded_segment_cannot_leave_its_position() {
        let base = url::Url::parse("https://example.test/").unwrap();
        for value in ["r?x", "r#x", "r%2fx", "a b", "x\\..\\y", "a\nb"] {
            let joined = base
                .join(&format!("rest/{}/child", enc(value).unwrap()))
                .unwrap();
            assert_eq!(joined.query(), None, "{value:?}");
            assert_eq!(joined.fragment(), None, "{value:?}");
            assert_eq!(joined.path_segments().unwrap().count(), 3, "{value:?}");
            assert!(joined.path().ends_with("/child"), "{value:?}");
        }
    }
}
