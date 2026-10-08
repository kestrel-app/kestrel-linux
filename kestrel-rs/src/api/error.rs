//! Errors raised by the Reolink API layer.

use std::fmt;

/// Session tokens travel in the query string, so they appear in transport error
/// text and in any URL that gets logged. Strip them before anything is shown or
/// recorded — a leaked token is a working credential until its lease expires.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let lower = text.to_ascii_lowercase();
    let mut cursor = 0usize;

    while let Some(found) = lower[cursor..].find("token=") {
        let start = cursor + found + "token=".len();
        out.push_str(&text[cursor..start]);
        out.push_str("<redacted>");
        // The value runs to the next delimiter.
        let end = text[start..]
            .find(|c: char| c == '&' || c.is_whitespace() || c == '\'' || c == '"')
            .map(|i| start + i)
            .unwrap_or(text.len());
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Mask the credentials in an RTSP URL for display or logging.
///
/// `rtsp://user:pass@host/path` embeds the password, so the URL must never be
/// printed, logged, or put in an error message as-is — decoder errors quote the
/// URL back at you, which is an easy way to leak a working credential.
pub fn redact_rtsp(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else { return url.to_string() };
    let after = scheme_end + 3;
    let Some(at) = url[after..].find('@') else { return url.to_string() };
    let creds = &url[after..after + at];
    let user = creds.split(':').next().unwrap_or("");
    format!("{}{user}:<redacted>@{}", &url[..after], &url[after + at + 1..])
}

#[derive(Debug)]
pub enum Error {
    /// The device could not be reached at all.
    Connection(String),
    /// Credentials were rejected, or the token expired and could not be renewed.
    Auth(String),
    /// The device answered but refused the command.
    Command {
        cmd: String,
        code: i64,
        detail: String,
        rsp_code: Option<i64>,
    },
    /// The device does not advertise the ability this call needs.
    Unsupported(String),
    /// The device answered, but not in a shape this client understands. Mostly
    /// a vendor whose API has moved, or a service sitting at that address that
    /// is not what the user thinks it is.
    Protocol(String),
}

impl Error {
    pub fn connection(msg: impl AsRef<str>) -> Self {
        Error::Connection(redact(msg.as_ref()))
    }

    pub fn auth(msg: impl AsRef<str>) -> Self {
        Error::Auth(redact(msg.as_ref()))
    }

    pub fn command(cmd: impl Into<String>, code: i64, detail: impl Into<String>) -> Self {
        Error::Command {
            cmd: cmd.into(),
            code,
            detail: detail.into(),
            rsp_code: None,
        }
    }

    /// True when the failure is the device refusing a command, as opposed to a
    /// transport problem. Callers retry those differently — see the Search
    /// `action` negotiation.
    pub fn is_command_failure(&self) -> bool {
        matches!(self, Error::Command { .. })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Connection(msg) => write!(f, "{msg}"),
            Error::Auth(msg) => write!(f, "{msg}"),
            Error::Command {
                cmd,
                detail,
                rsp_code,
                ..
            } => {
                let detail = if detail.is_empty() {
                    "unknown error"
                } else {
                    detail
                };
                match rsp_code {
                    Some(rsp) => write!(f, "{cmd} failed: {detail} (rspCode {rsp})"),
                    None => write!(f, "{cmd} failed: {detail}"),
                }
            }
            Error::Unsupported(msg) => write!(f, "{msg}"),
            Error::Protocol(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_token_from_a_url() {
        let text = "502 Server Error for url: \
                    http://192.0.2.242:80/cgi-bin/api.cgi?cmd=Search&token=07d67b14478a1bf";
        let out = redact(text);
        assert!(!out.contains("07d67b14478a1bf"), "{out}");
        assert!(out.contains("token=<redacted>"), "{out}");
        // Everything else must survive.
        assert!(out.contains("cmd=Search"), "{out}");
    }

    #[test]
    fn redacts_regardless_of_delimiter_or_case() {
        for probe in [
            "token=abc&next=1",
            "'token=abc'",
            "TOKEN=ABC",
            "token=abc then more text",
        ] {
            let out = redact(probe);
            assert!(!out.to_ascii_lowercase().contains("abc"), "{probe} -> {out}");
        }
        assert_eq!(redact("nothing to see"), "nothing to see");
    }

    #[test]
    fn masks_rtsp_credentials() {
        assert_eq!(
            redact_rtsp("rtsp://admin:s3cr3t%2Ax@192.0.2.242:554/h264Preview_01_sub"),
            "rtsp://admin:<redacted>@192.0.2.242:554/h264Preview_01_sub"
        );
        // Nothing to mask, nothing changed.
        assert_eq!(redact_rtsp("rtsp://192.0.2.242:554/x"), "rtsp://192.0.2.242:554/x");
        assert_eq!(redact_rtsp("not a url"), "not a url");
    }

    #[test]
    fn keeps_trailing_parameters() {
        assert_eq!(redact("a?token=xyz&cmd=Snap"), "a?token=<redacted>&cmd=Snap");
    }
}
