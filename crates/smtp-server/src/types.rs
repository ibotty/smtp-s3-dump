//! Validated newtypes for SMTP protocol elements.
//!
//! General rule: constructors validate, fields are private, accessors expose
//! the validated data.

use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroUsize;

use smtp_proto::{MailFrom, RcptTo};

use crate::reply::Rejection;

/// A validated DNS hostname: LDH labels, at most 253 bytes, no empty labels.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hostname(String);

/// Error returned by [`Hostname::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidHostname(pub(crate) &'static str);

impl fmt::Display for InvalidHostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid hostname: {}", self.0)
    }
}
impl std::error::Error for InvalidHostname {}

impl Hostname {
    pub fn new(s: &str) -> Result<Self, InvalidHostname> {
        if s.is_empty() || s.len() > 253 {
            return Err(InvalidHostname("length must be 1..=253"));
        }
        for label in s.split('.') {
            if label.is_empty() || label.len() > 63 {
                return Err(InvalidHostname("label length must be 1..=63"));
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(InvalidHostname("label must not start/end with '-'"));
            }
            if !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(InvalidHostname(
                    "label must be LDH (letters, digits, hyphen)",
                ));
            }
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The domain part of a mailbox: a hostname, or an address literal (`[1.2.3.4]`, `[IPv6:::1]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Domain {
    Name(Hostname),
    Literal(IpAddr),
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Domain::Name(h) => f.write_str(h.as_str()),
            Domain::Literal(IpAddr::V4(ip)) => write!(f, "[{ip}]"),
            Domain::Literal(IpAddr::V6(ip)) => write!(f, "[IPv6:{ip}]"),
        }
    }
}

impl Domain {
    pub(crate) fn parse(s: &str) -> Result<Self, InvalidHostname> {
        if let Some(inner) = s.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let addr = inner.strip_prefix("IPv6:").unwrap_or(inner);
            inner
                .parse::<IpAddr>()
                .or_else(|_| addr.parse::<IpAddr>())
                .map(Domain::Literal)
                .map_err(|_| InvalidHostname("invalid address literal"))
        } else {
            Hostname::new(s).map(Domain::Name)
        }
    }
}

/// The local part of a mailbox (before the `@`), in wire form: a dot-atom, or a
/// quoted-string *including* its surrounding quotes and any `\` quoted-pairs.
/// Validated per RFC 5321 §4.1.2 (atext / qtext / quoted-pair, no control
/// characters). Non-ASCII UTF-8 is accepted as in RFC 6531; whether it is legal
/// is a per-transaction SMTPUTF8 decision enforced where the mailbox is built.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalPart(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidLocalPart(pub(crate) &'static str);

impl fmt::Display for InvalidLocalPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid local part: {}", self.0)
    }
}
impl std::error::Error for InvalidLocalPart {}

impl LocalPart {
    pub fn new(s: &str) -> Result<Self, InvalidLocalPart> {
        if s.is_empty() {
            return Err(InvalidLocalPart("must not be empty"));
        }
        if let Some(inner) = s.strip_prefix('"') {
            let inner = inner
                .strip_suffix('"')
                .ok_or(InvalidLocalPart("unterminated quoted-string"))?;
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                let ok = match c {
                    '\\' => chars.next().is_some_and(|q| matches!(q, ' '..='~')),
                    '"' => false,
                    ' '..='~' => true,
                    c => !c.is_ascii() && !c.is_control(),
                };
                if !ok {
                    return Err(InvalidLocalPart("invalid character in quoted-string"));
                }
            }
        } else {
            for label in s.split('.') {
                if label.is_empty() {
                    return Err(InvalidLocalPart("dot-atom must not have empty labels"));
                }
                let atext = |c: char| {
                    if c.is_ascii() {
                        c.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~".contains(c)
                    } else {
                        !c.is_control()
                    }
                };
                if !label.chars().all(atext) {
                    return Err(InvalidLocalPart("invalid character in dot-atom"));
                }
            }
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LocalPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated `local@domain` mailbox address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    local: LocalPart,
    domain: Domain,
}

impl Mailbox {
    pub fn new(local: LocalPart, domain: Domain) -> Self {
        Self { local, domain }
    }

    pub fn local(&self) -> &LocalPart {
        &self.local
    }

    pub fn domain(&self) -> &Domain {
        &self.domain
    }

    /// Split `local@domain` (or `local@[literal]`) on the single unquoted `@`.
    pub(crate) fn parse(addr: &str) -> Result<Self, Rejection> {
        let invalid = || Rejection::invalid_mailbox(addr);
        let mut in_quotes = false;
        let mut at = None;
        let mut chars = addr.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' if in_quotes => {
                    chars.next();
                }
                '"' => in_quotes = !in_quotes,
                '@' if !in_quotes => at = Some(i),
                _ => {}
            }
        }
        let at = at.ok_or_else(invalid)?;
        let local = LocalPart::new(&addr[..at]).map_err(|_| invalid())?;
        let domain = Domain::parse(&addr[at + 1..]).map_err(|_| invalid())?;
        Ok(Mailbox::new(local, domain))
    }
}

impl fmt::Display for Mailbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.local, self.domain)
    }
}

/// `MAIL FROM:<...>` address: either the null reverse-path (`<>`) or a mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReversePath {
    Null,
    Mailbox(Mailbox),
}

impl fmt::Display for ReversePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReversePath::Null => f.write_str("<>"),
            ReversePath::Mailbox(m) => write!(f, "{m}"),
        }
    }
}

/// `RCPT TO:<...>` address: either `<postmaster>` or a mailbox (RFC 5321 §4.1.1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardPath {
    Postmaster,
    Mailbox(Mailbox),
}

impl fmt::Display for ForwardPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForwardPath::Postmaster => f.write_str("postmaster"),
            ForwardPath::Mailbox(m) => write!(f, "{m}"),
        }
    }
}

/// `BODY=` parameter of `MAIL FROM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    SevenBit,
    EightBitMime,
    BinaryMime,
}

/// `RET=` parameter of `MAIL FROM` (RFC 3461 DSN).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ret {
    Full,
    Headers,
}

/// xtext-decoded `ENVID=` parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvId(String);

impl EnvId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated `MAIL FROM` transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    path: ReversePath,
    size: Option<NonZeroUsize>,
    body: Body,
    ret: Option<Ret>,
    env_id: Option<EnvId>,
    smtputf8: bool,
}

impl Sender {
    pub fn path(&self) -> &ReversePath {
        &self.path
    }

    pub fn size(&self) -> Option<NonZeroUsize> {
        self.size
    }

    pub fn body(&self) -> Body {
        self.body
    }

    pub fn ret(&self) -> Option<Ret> {
        self.ret
    }

    pub fn env_id(&self) -> Option<&EnvId> {
        self.env_id.as_ref()
    }

    pub fn smtputf8(&self) -> bool {
        self.smtputf8
    }

    pub(crate) fn from_smtp(
        from: MailFrom<String>,
        dsn_enabled: bool,
        smtputf8_enabled: bool,
        eightbitmime_enabled: bool,
    ) -> Result<Self, Rejection> {
        let path = if from.address.is_empty() {
            ReversePath::Null
        } else {
            ReversePath::Mailbox(Mailbox::parse(&from.address)?)
        };

        let body = match from.flags
            & (smtp_proto::MAIL_BODY_8BITMIME | smtp_proto::MAIL_BODY_BINARYMIME)
        {
            0 => Body::SevenBit,
            f if f == smtp_proto::MAIL_BODY_8BITMIME => Body::EightBitMime,
            f if f == smtp_proto::MAIL_BODY_BINARYMIME => Body::BinaryMime,
            _ => return Err(Rejection::unsupported_param("BODY")),
        };
        if body != Body::SevenBit && !eightbitmime_enabled {
            return Err(Rejection::unsupported_param("BODY"));
        }

        let smtputf8 = from.flags & smtp_proto::MAIL_SMTPUTF8 != 0;
        if smtputf8 && !smtputf8_enabled {
            return Err(Rejection::unsupported_param("SMTPUTF8"));
        }

        let ret = match from.flags & (smtp_proto::MAIL_RET_FULL | smtp_proto::MAIL_RET_HDRS) {
            0 => None,
            f if f == smtp_proto::MAIL_RET_FULL => Some(Ret::Full),
            f if f == smtp_proto::MAIL_RET_HDRS => Some(Ret::Headers),
            _ => return Err(Rejection::unsupported_param("RET")),
        };
        if ret.is_some() && !dsn_enabled {
            return Err(Rejection::unsupported_param("RET"));
        }

        let env_id = match from.env_id {
            Some(raw) if dsn_enabled => Some(EnvId(xtext_decode(&raw)?)),
            Some(_) => return Err(Rejection::unsupported_param("ENVID")),
            None => None,
        };

        Ok(Sender {
            path,
            size: NonZeroUsize::new(from.size),
            body,
            ret,
            env_id,
            smtputf8,
        })
    }
}

/// `NOTIFY=` parameter of `RCPT TO` (RFC 3461 DSN). `NEVER` combined with any
/// other keyword, or `SUCCESS,FAILURE,DELAY` all disabled, is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    Default,
    Never,
    On {
        success: bool,
        failure: bool,
        delay: bool,
    },
}

/// xtext-decoded `ORCPT=` parameter: `addr-type;xtext-addr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginalRecipient {
    addr_type: String,
    addr: String,
}

impl OriginalRecipient {
    pub fn addr_type(&self) -> &str {
        &self.addr_type
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }
}

/// A validated `RCPT TO` recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    path: ForwardPath,
    notify: Notify,
    orcpt: Option<OriginalRecipient>,
}

impl Recipient {
    pub fn path(&self) -> &ForwardPath {
        &self.path
    }

    pub fn notify(&self) -> Notify {
        self.notify
    }

    pub fn orcpt(&self) -> Option<&OriginalRecipient> {
        self.orcpt.as_ref()
    }

    pub(crate) fn from_smtp(to: RcptTo<String>, dsn_enabled: bool) -> Result<Self, Rejection> {
        let path = if to.address.is_empty() {
            ForwardPath::Postmaster
        } else {
            ForwardPath::Mailbox(Mailbox::parse(&to.address)?)
        };

        const ALL: u64 = smtp_proto::RCPT_NOTIFY_SUCCESS
            | smtp_proto::RCPT_NOTIFY_FAILURE
            | smtp_proto::RCPT_NOTIFY_DELAY
            | smtp_proto::RCPT_NOTIFY_NEVER;
        let notify_flags = to.flags & ALL;
        let notify = if notify_flags == 0 {
            Notify::Default
        } else if notify_flags == smtp_proto::RCPT_NOTIFY_NEVER {
            Notify::Never
        } else if notify_flags & smtp_proto::RCPT_NOTIFY_NEVER != 0 {
            return Err(Rejection::invalid_notify());
        } else {
            let success = notify_flags & smtp_proto::RCPT_NOTIFY_SUCCESS != 0;
            let failure = notify_flags & smtp_proto::RCPT_NOTIFY_FAILURE != 0;
            let delay = notify_flags & smtp_proto::RCPT_NOTIFY_DELAY != 0;
            Notify::On {
                success,
                failure,
                delay,
            }
        };
        if !matches!(notify, Notify::Default) && !dsn_enabled {
            return Err(Rejection::unsupported_param("NOTIFY"));
        }

        let orcpt = match to.orcpt {
            Some(raw) if dsn_enabled => Some(parse_orcpt(&raw)?),
            Some(_) => return Err(Rejection::unsupported_param("ORCPT")),
            None => None,
        };

        Ok(Recipient {
            path,
            notify,
            orcpt,
        })
    }
}

fn parse_orcpt(raw: &str) -> Result<OriginalRecipient, Rejection> {
    let (addr_type, addr) = raw
        .split_once(';')
        .ok_or_else(|| Rejection::unsupported_param("ORCPT"))?;
    Ok(OriginalRecipient {
        addr_type: addr_type.to_owned(),
        addr: xtext_decode(addr)?,
    })
}

/// RFC 3461 xtext decoding: `+HH` is a hex-escaped byte, everything else is literal.
fn xtext_decode(s: &str) -> Result<String, Rejection> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| Rejection::unsupported_param("xtext"))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Rejection::unsupported_param("xtext"))
}

/// A non-empty, singly-growable list. `RCPT` guarantees at least one recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T> {
    first: T,
    rest: Vec<T>,
}

impl<T> NonEmpty<T> {
    pub fn new(first: T) -> Self {
        Self {
            first,
            rest: Vec::new(),
        }
    }

    pub fn push(&mut self, item: T) {
        self.rest.push(item);
    }

    pub fn first(&self) -> &T {
        &self.first
    }

    pub fn len(&self) -> NonZeroUsize {
        NonZeroUsize::new(1 + self.rest.len()).expect("1 + n is never zero")
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        std::iter::once(&self.first).chain(self.rest.iter())
    }
}

/// A complete transaction: one sender, one or more recipients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    sender: Sender,
    rcpts: NonEmpty<Recipient>,
}

impl Envelope {
    pub(crate) fn new(sender: Sender, rcpts: NonEmpty<Recipient>) -> Self {
        Self { sender, rcpts }
    }

    pub fn sender(&self) -> &Sender {
        &self.sender
    }

    pub fn rcpts(&self) -> &NonEmpty<Recipient> {
        &self.rcpts
    }

    pub(crate) fn push_rcpt(&mut self, rcpt: Recipient) {
        self.rcpts.push(rcpt);
    }
}

/// A configured message-size limit (`SIZE=` in EHLO, never zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageSize(NonZeroUsize);

impl MessageSize {
    pub fn new(size: usize) -> Option<Self> {
        NonZeroUsize::new(size).map(Self)
    }

    pub fn get(&self) -> usize {
        self.0.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_accepts_ldh() {
        assert!(Hostname::new("mx.example.org").is_ok());
        assert!(Hostname::new("a").is_ok());
    }

    #[test]
    fn hostname_rejects_bad_labels() {
        assert!(Hostname::new("").is_err());
        assert!(Hostname::new(".example.org").is_err());
        assert!(Hostname::new("-example.org").is_err());
        assert!(Hostname::new("exam_ple.org").is_err());
        assert!(Hostname::new(&"a".repeat(254)).is_err());
    }

    #[test]
    fn domain_parses_literals() {
        assert_eq!(
            Domain::parse("[1.2.3.4]").unwrap(),
            Domain::Literal("1.2.3.4".parse().unwrap())
        );
        assert_eq!(
            Domain::parse("[IPv6:::1]").unwrap(),
            Domain::Literal("::1".parse().unwrap())
        );
        assert!(matches!(
            Domain::parse("example.org").unwrap(),
            Domain::Name(_)
        ));
    }

    #[test]
    fn mailbox_splits_on_unquoted_at() {
        let m = Mailbox::parse("foo@example.org").unwrap();
        assert_eq!(m.local().as_str(), "foo");
        assert_eq!(
            m.domain(),
            &Domain::Name(Hostname::new("example.org").unwrap())
        );
    }

    #[test]
    fn null_reverse_path_from_empty_address() {
        let from = MailFrom {
            address: String::new(),
            ..Default::default()
        };
        let sender = Sender::from_smtp(from, false, false, false).unwrap();
        assert_eq!(sender.path(), &ReversePath::Null);
    }

    #[test]
    fn postmaster_forward_path_from_empty_address() {
        let to = RcptTo {
            address: String::new(),
            ..Default::default()
        };
        let rcpt = Recipient::from_smtp(to, false).unwrap();
        assert_eq!(rcpt.path(), &ForwardPath::Postmaster);
    }

    #[test]
    fn notify_never_combined_is_rejected() {
        let to = RcptTo {
            address: "a@b".to_owned(),
            flags: smtp_proto::RCPT_NOTIFY_NEVER | smtp_proto::RCPT_NOTIFY_SUCCESS,
            ..Default::default()
        };
        assert!(Recipient::from_smtp(to, false).is_err());
    }

    #[test]
    fn local_part_validation() {
        for ok in [
            "foo",
            "a.b",
            "a+b/c",
            "\"a b\"",
            "\"a\\\"b\"",
            "\"a@b\"",
            "\"\"",
            "é.x",
            "\"é\"",
        ] {
            assert!(LocalPart::new(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "a..b",
            ".a",
            "a b",
            "a@b",
            "a\\b",
            "a\x00",
            "a\x1b[0m",
            "\"a\"b\"",
            "\"a",
            "\"",
            "\"a\\\"",
            "\"a\x00b\"",
            "\"a\x1bb\"",
            "\"a\x7fb\"",
            "\"a\\\x00\"",
            "a\u{85}",
            "\"a\u{9b}\"",
        ] {
            assert!(LocalPart::new(bad).is_err(), "{bad:?}");
        }
    }

    use proptest::prelude::*;

    /// Strings biased towards SMTP-relevant characters (`@`, `.`, `"`, `\`, control
    /// chars, hyphen) plus plain ASCII alphanumerics, rather than mostly-irrelevant
    /// arbitrary Unicode.
    fn smtp_ish_string() -> impl Strategy<Value = String> {
        proptest::string::string_regex(r#"[a-zA-Z0-9@.\x22\\ \x00-\x1f-]{0,20}"#).unwrap()
    }

    fn local_part_string() -> impl Strategy<Value = String> {
        proptest::string::string_regex(r#"[a-z0-9@.\x22\\ /!#\x00-\x1f\x7f\u{80}-\u{9f}é-]{0,20}"#)
            .unwrap()
    }

    proptest! {
        #[test]
        fn hostname_new_no_panic_and_roundtrips(s in smtp_ish_string()) {
            if let Ok(h) = Hostname::new(&s) {
                prop_assert!(Hostname::new(&h.to_string()).is_ok());
            }
        }

        #[test]
        fn local_part_new_no_panic_and_roundtrips(s in smtp_ish_string()) {
            if let Ok(lp) = LocalPart::new(&s) {
                prop_assert!(LocalPart::new(&lp.to_string()).is_ok());
            }
        }

        #[test]
        fn mailbox_parse_no_panic(s in smtp_ish_string()) {
            let _ = Mailbox::parse(&s);
        }

        #[test]
        fn accepted_local_part_has_no_controls_and_roundtrips(s in local_part_string()) {
            if let Ok(lp) = LocalPart::new(&s) {
                let shown = lp.to_string();
                prop_assert!(!shown.chars().any(char::is_control));
                let m = Mailbox::parse(&format!("{shown}@example.org")).unwrap();
                prop_assert_eq!(m.local(), &lp);
            }
        }
    }
}
