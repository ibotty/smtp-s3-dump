//! Negative replies (4xx/5xx). Positive replies are built by the core only.

/// Replaces every control character (notably CR/LF) with a space, so reply text can
/// never terminate its line early or inject further replies.
pub(crate) fn sanitize_text(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A negative (4xx/5xx) reply a [`crate::Handler`] can return to fail a step of the
/// transaction. A 421 additionally closes the connection after it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    code: u16,
    /// Enhanced status code (RFC 3463): `class.subject.detail`.
    enhanced: (u8, u8, u8),
    text: String,
}

impl Rejection {
    /// A 421 closes the session after being sent.
    pub(crate) fn closes_session(&self) -> bool {
        self.code == 421
    }

    fn build(code: u16, class: u8, subject: u8, detail: u8, text: impl Into<String>) -> Self {
        Self {
            code,
            enhanced: (class, subject, detail),
            text: sanitize_text(&text.into()),
        }
    }

    /// 451 4.3.0 — transient local error, client may retry.
    pub fn transient(msg: impl Into<String>) -> Self {
        Self::build(451, 4, 3, 0, msg)
    }

    /// 550 5.1.1 — mailbox does not exist / is not local.
    pub fn mailbox_unavailable(msg: impl Into<String>) -> Self {
        Self::build(550, 5, 1, 1, msg)
    }

    /// 550 5.7.1 — delivery not authorized.
    pub fn not_authorized(msg: impl Into<String>) -> Self {
        Self::build(550, 5, 7, 1, msg)
    }

    /// 554 5.6.0 — message content is unacceptable; retrying will not help.
    pub fn invalid_content(msg: impl Into<String>) -> Self {
        Self::build(554, 5, 6, 0, msg)
    }

    /// 552 5.3.4 — message too big.
    pub fn too_big() -> Self {
        Self::build(552, 5, 3, 4, "message too big")
    }

    /// 421 4.3.0 — service not available, closes the connection after being sent.
    pub fn closing(msg: impl Into<String>) -> Self {
        Self::build(421, 4, 3, 0, msg)
    }

    pub(crate) fn too_many_rcpts() -> Self {
        Self::build(452, 4, 5, 3, "too many recipients")
    }

    pub(crate) fn syntax_error(msg: impl Into<String>) -> Self {
        Self::build(501, 5, 5, 4, msg)
    }

    pub(crate) fn invalid_mailbox(addr: &str) -> Self {
        Self::build(
            553,
            5,
            1,
            3,
            format!("mailbox address {addr:?} is malformed"),
        )
    }

    pub(crate) fn invalid_notify() -> Self {
        Self::build(
            501,
            5,
            5,
            4,
            "NOTIFY=NEVER cannot be combined with other keywords",
        )
    }

    pub(crate) fn unsupported_param(name: &str) -> Self {
        Self::build(555, 5, 5, 4, format!("unsupported parameter: {name}"))
    }

    pub(crate) fn command_syntax(usage: &str) -> Self {
        Self::build(501, 5, 5, 4, format!("syntax: {usage}"))
    }

    pub(crate) fn bad_sequence(msg: impl Into<String>) -> Self {
        Self::build(503, 5, 5, 1, msg)
    }

    pub(crate) fn not_implemented() -> Self {
        Self::build(502, 5, 5, 1, "command not implemented")
    }

    pub(crate) fn unknown_command() -> Self {
        Self::build(500, 5, 5, 2, "unrecognized command")
    }

    pub(crate) fn line_too_long() -> Self {
        Self::build(500, 5, 5, 2, "line too long")
    }

    pub(crate) fn too_many_bad_commands() -> Self {
        Self::build(421, 4, 3, 0, "too many unrecognized commands")
    }

    /// Render as a single SMTP reply line. Control characters (including CR/LF) in the text
    /// are replaced with spaces on construction; multi-line text is not supported.
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        let resp = smtp_proto::Response::new(
            self.code,
            self.enhanced.0,
            self.enhanced.1,
            self.enhanced.2,
            self.text.as_str(),
        );
        resp.write(out).expect("Vec<u8> writes are infallible");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn rendered_reply_is_single_crlf_terminated_line(text in ".{0,40}", junk in "[\r\n\x00-\x1f]{0,6}") {
            let text = format!("{junk}{text}\r\n250 injected{junk}");
            let mut out = Vec::new();
            Rejection::transient(text.as_str()).write(&mut out);
            prop_assert!(out.ends_with(b"\r\n"));
            let body = &out[..out.len() - 2];
            prop_assert!(!body.iter().any(|b| *b == b'\r' || *b == b'\n'));
            prop_assert_eq!(out.iter().filter(|b| **b == b'\n').count(), 1);
        }
    }
}
