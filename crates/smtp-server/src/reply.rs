//! Negative replies (4xx/5xx). Positive replies are built by the core only.

use std::fmt;

/// The numeric SMTP reply code of a [`Rejection`]: 4xx (transient) or 5xx (permanent) only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RejectCode(u16);

impl RejectCode {
    pub fn new(code: u16) -> Result<Self, InvalidRejection> {
        match code {
            400..=599 => Ok(Self(code)),
            _ => Err(InvalidRejection("code must be 4xx or 5xx")),
        }
    }

    pub fn get(&self) -> u16 {
        self.0
    }

    fn class(&self) -> u8 {
        (self.0 / 100) as u8
    }
}

/// The enhanced status code (RFC 3463) of a [`Rejection`]: `class.subject.detail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnhancedCode {
    class: u8,
    subject: u8,
    detail: u8,
}

impl EnhancedCode {
    pub fn new(class: u8, subject: u8, detail: u8) -> Result<Self, InvalidRejection> {
        if !matches!(class, 4 | 5) || subject > 9 || detail > 9 {
            return Err(InvalidRejection(
                "enhanced code class must be 4 or 5, subject/detail 0..=9",
            ));
        }
        Ok(Self {
            class,
            subject,
            detail,
        })
    }

    pub fn parts(&self) -> (u8, u8, u8) {
        (self.class, self.subject, self.detail)
    }
}

impl fmt::Display for EnhancedCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.class, self.subject, self.detail)
    }
}

/// Error returned by [`Rejection::new`], [`RejectCode::new`] and [`EnhancedCode::new`]:
/// only 4xx/5xx codes with a matching enhanced-code class are valid rejections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidRejection(pub(crate) &'static str);

impl fmt::Display for InvalidRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid rejection: {}", self.0)
    }
}
impl std::error::Error for InvalidRejection {}

/// A negative (4xx/5xx) reply a [`crate::Handler`] can return to fail a step of the
/// transaction. A 421 additionally closes the connection after it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    code: RejectCode,
    enhanced: EnhancedCode,
    text: String,
}

impl Rejection {
    pub fn new(
        code: RejectCode,
        enhanced: EnhancedCode,
        text: impl Into<String>,
    ) -> Result<Self, InvalidRejection> {
        if code.class() != enhanced.class {
            return Err(InvalidRejection(
                "reply code class must match enhanced code class",
            ));
        }
        Ok(Self {
            code,
            enhanced,
            text: text.into(),
        })
    }

    pub fn code(&self) -> RejectCode {
        self.code
    }

    pub fn enhanced(&self) -> EnhancedCode {
        self.enhanced
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// A 421 closes the session after being sent.
    pub fn closes_session(&self) -> bool {
        self.code.get() == 421
    }

    fn build(code: u16, class: u8, subject: u8, detail: u8, text: impl Into<String>) -> Self {
        Self {
            code: RejectCode(code),
            enhanced: EnhancedCode {
                class,
                subject,
                detail,
            },
            text: text.into(),
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

    /// Render as an SMTP reply line (or, for multi-line text, several lines with `-` continuations).
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        let resp = smtp_proto::Response::new(
            self.code.get(),
            self.enhanced.class,
            self.enhanced.subject,
            self.enhanced.detail,
            self.text.as_str(),
        );
        resp.write(out).expect("Vec<u8> writes are infallible");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_2xx_3xx() {
        assert!(RejectCode::new(250).is_err());
        assert!(RejectCode::new(354).is_err());
        assert!(RejectCode::new(450).is_ok());
        assert!(RejectCode::new(550).is_ok());
    }

    #[test]
    fn new_rejects_class_mismatch() {
        let code = RejectCode::new(450).unwrap();
        let enhanced = EnhancedCode::new(5, 1, 1).unwrap();
        assert!(Rejection::new(code, enhanced, "x").is_err());
    }

    #[test]
    fn enhanced_code_rejects_bad_class() {
        assert!(EnhancedCode::new(2, 0, 0).is_err());
        assert!(EnhancedCode::new(4, 10, 0).is_err());
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn reject_code_roundtrips_or_errors(code: u16) {
            if let Ok(rc) = RejectCode::new(code) {
                prop_assert_eq!(rc.get(), code);
                prop_assert!((4..=5).contains(&rc.class()));
            }
        }

        #[test]
        fn enhanced_code_roundtrips_or_errors(class: u8, subject: u8, detail: u8) {
            if let Ok(ec) = EnhancedCode::new(class, subject, detail) {
                prop_assert_eq!(ec.parts(), (class, subject, detail));
                prop_assert!(class == 4 || class == 5);
            }
        }

        #[test]
        fn rejection_new_matches_class_invariant(
            code in 400u16..=599,
            class in 4u8..=5,
            subject in 0u8..=9,
            detail in 0u8..=9,
        ) {
            let rc = RejectCode::new(code).unwrap();
            let ec = EnhancedCode::new(class, subject, detail).unwrap();
            prop_assert_eq!(Rejection::new(rc, ec, "x").is_ok(), rc.class() == ec.parts().0);
        }
    }
}
