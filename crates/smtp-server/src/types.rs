//! Validated newtypes for SMTP protocol elements.
//!
//! General rule: constructors validate, fields are private, accessors expose
//! the validated data.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroUsize;

use smtp_proto::{MailFrom, RcptTo};

use crate::Config;
use crate::reply::Rejection;

/// A validated DNS hostname: LDH labels, at most 253 bytes, no empty labels.
///
/// [`Hostname::new`] is strict (letters, digits, hyphen). Hostnames inside a
/// client-supplied EHLO/HELO [`Domain`] are additionally allowed to contain `_`
/// in labels, since real clients send such names; they are never ASCII-unsafe
/// (no whitespace, control characters or non-ASCII).
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

/// Whether `_` is accepted in hostname labels.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Underscore {
    Reject,
    Allow,
}

impl Hostname {
    /// Strict LDH hostname.
    pub fn new(s: &str) -> Result<Self, InvalidHostname> {
        Self::parse(s, Underscore::Reject)
    }

    fn parse(s: &str, underscore: Underscore) -> Result<Self, InvalidHostname> {
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
            if !label.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || b == b'-'
                    || (underscore == Underscore::Allow && b == b'_')
            }) {
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

/// A domain: a hostname, or an address literal (`[1.2.3.4]`, `[IPv6:::1]`). Used for
/// the domain part of a mailbox and for the client-supplied EHLO/HELO argument.
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
    /// Mailbox domain: strict LDH hostname or RFC 5321 address literal.
    pub(crate) fn parse(s: &str) -> Result<Self, InvalidHostname> {
        Self::parse_with(s, Underscore::Reject)
    }

    /// Client-supplied EHLO/HELO domain: like [`Domain::parse`], but hostname
    /// labels may contain `_`.
    pub(crate) fn parse_client(s: &str) -> Result<Self, InvalidHostname> {
        Self::parse_with(s, Underscore::Allow)
    }

    /// Literals per RFC 5321 §4.1.3: `[a.b.c.d]` or `[IPv6:...]` only.
    fn parse_with(s: &str, underscore: Underscore) -> Result<Self, InvalidHostname> {
        let Some(inner) = s.strip_prefix('[') else {
            return Hostname::parse(s, underscore).map(Domain::Name);
        };
        let bad = || InvalidHostname("invalid address literal");
        let inner = inner.strip_suffix(']').ok_or_else(bad)?;
        match inner.split_once(':') {
            Some((tag, v6)) if tag.eq_ignore_ascii_case("IPv6") => v6
                .parse::<Ipv6Addr>()
                .map(|ip| Domain::Literal(ip.into()))
                .map_err(|_| bad()),
            Some(_) => Err(bad()),
            None => inner
                .parse::<Ipv4Addr>()
                .map(|ip| Domain::Literal(ip.into()))
                .map_err(|_| bad()),
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

/// A validated `MAIL FROM` transaction. `BODY=`, `SMTPUTF8`, `RET=` and `ENVID=` are checked
/// against the enabled extensions but not retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    path: ReversePath,
    size: Option<NonZeroUsize>,
}

impl Sender {
    pub fn path(&self) -> &ReversePath {
        &self.path
    }

    pub fn size(&self) -> Option<NonZeroUsize> {
        self.size
    }

    pub(crate) fn from_smtp(from: MailFrom<String>, cfg: &Config) -> Result<Self, Rejection> {
        let path = if from.address.is_empty() {
            ReversePath::Null
        } else {
            ReversePath::Mailbox(Mailbox::parse(&from.address)?)
        };

        const BODY: u64 = smtp_proto::MAIL_BODY_8BITMIME | smtp_proto::MAIL_BODY_BINARYMIME;
        const RET: u64 = smtp_proto::MAIL_RET_FULL | smtp_proto::MAIL_RET_HDRS;
        let body = from.flags & BODY;
        if body == BODY || (body != 0 && !cfg.eightbitmime) {
            return Err(Rejection::unsupported_param("BODY"));
        }
        if from.flags & smtp_proto::MAIL_SMTPUTF8 != 0 && !cfg.smtputf8 {
            return Err(Rejection::unsupported_param("SMTPUTF8"));
        }
        let ret = from.flags & RET;
        if ret == RET || (ret != 0 && !cfg.dsn) {
            return Err(Rejection::unsupported_param("RET"));
        }
        if from.env_id.is_some() && !cfg.dsn {
            return Err(Rejection::unsupported_param("ENVID"));
        }

        Ok(Sender {
            path,
            size: NonZeroUsize::new(from.size),
        })
    }
}

/// A validated `RCPT TO` recipient. `NOTIFY=` and `ORCPT=` are checked against the enabled
/// extensions but not retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    path: ForwardPath,
}

impl Recipient {
    pub fn path(&self) -> &ForwardPath {
        &self.path
    }

    pub(crate) fn from_smtp(to: RcptTo<String>, cfg: &Config) -> Result<Self, Rejection> {
        let path = if to.address.is_empty() {
            ForwardPath::Postmaster
        } else {
            ForwardPath::Mailbox(Mailbox::parse(&to.address)?)
        };

        const NEVER: u64 = smtp_proto::RCPT_NOTIFY_NEVER;
        const ALL: u64 = smtp_proto::RCPT_NOTIFY_SUCCESS
            | smtp_proto::RCPT_NOTIFY_FAILURE
            | smtp_proto::RCPT_NOTIFY_DELAY
            | NEVER;
        let notify = to.flags & ALL;
        if notify & NEVER != 0 && notify != NEVER {
            return Err(Rejection::invalid_notify());
        }
        if notify != 0 && !cfg.dsn {
            return Err(Rejection::unsupported_param("NOTIFY"));
        }
        if to.orcpt.is_some() && !cfg.dsn {
            return Err(Rejection::unsupported_param("ORCPT"));
        }

        Ok(Recipient { path })
    }
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

    fn test_cfg() -> Config {
        Config::new(Hostname::new("mx.example.org").unwrap())
    }

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
    fn domain_literal_table() {
        for ok in [
            "[1.2.3.4]",
            "[203.0.113.7]",
            "[IPv6:::1]",
            "[IPv6:2001:db8::1]",
            "[IPv6:::ffff:1.2.3.4]",
            "[ipv6:::1]",
        ] {
            assert!(
                matches!(Domain::parse(ok), Ok(Domain::Literal(_))),
                "{ok:?}"
            );
            assert!(
                matches!(Domain::parse_client(ok), Ok(Domain::Literal(_))),
                "{ok:?}"
            );
        }
        for bad in [
            "[]",
            "[",
            "[1.2.3.4",
            "[1.2.3]",
            "[1.2.3.4.5]",
            "[256.1.1.1]",
            "[::1]",
            "[2001:db8::1]",
            "[IPv6:1.2.3.4]",
            "[IPv6:]",
            "[IPv6:::1",
            "[IPv4:1.2.3.4]",
            "[IPv6 :::1]",
            "[ 1.2.3.4]",
            "[1.2.3.4 ]",
            "[1.2.3.4]x",
            "[IPv6:fe80::1%eth0]",
            "[1.2.3.4\r\n]",
            "[[1.2.3.4]]",
        ] {
            assert!(Domain::parse(bad).is_err(), "{bad:?}");
            assert!(Domain::parse_client(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn client_domain_allows_underscore_only_when_lenient() {
        for ok in ["my_host.example", "_dmarc.example.org", "a_b", "a_"] {
            assert!(
                matches!(Domain::parse_client(ok), Ok(Domain::Name(_))),
                "{ok:?}"
            );
            assert!(Domain::parse(ok).is_err(), "{ok:?}");
            assert!(Hostname::new(ok).is_err(), "{ok:?}");
        }
        let long_label = format!("{}.example", "a".repeat(64));
        let long_name = format!("{}.{}", "a".repeat(63), "b_".repeat(100));
        for bad in [
            "",
            ".",
            "a..b",
            ".a",
            "a.",
            "-a_b",
            "a_b-",
            "a b",
            "a\tb",
            "a\x00",
            "a\r\n",
            "a\x7f",
            "é_x",
            "a_é",
            "a\u{85}",
            "a:b",
            "a@b",
            long_label.as_str(),
            long_name.as_str(),
        ] {
            assert!(Domain::parse_client(bad).is_err(), "{bad:?}");
        }
        assert!(Domain::parse_client(&format!("{}.example", "a_".repeat(31))).is_ok());
        assert!(Domain::parse_client(&format!("{}.example", "_".repeat(63))).is_ok());
        assert!(Domain::parse_client(&"a".repeat(254)).is_err());
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
        let sender = Sender::from_smtp(from, &test_cfg()).unwrap();
        assert_eq!(sender.path(), &ReversePath::Null);
    }

    #[test]
    fn postmaster_forward_path_from_empty_address() {
        let to = RcptTo {
            address: String::new(),
            ..Default::default()
        };
        let rcpt = Recipient::from_smtp(to, &test_cfg()).unwrap();
        assert_eq!(rcpt.path(), &ForwardPath::Postmaster);
    }

    #[test]
    fn notify_never_combined_is_rejected() {
        let to = RcptTo {
            address: "a@b".to_owned(),
            flags: smtp_proto::RCPT_NOTIFY_NEVER | smtp_proto::RCPT_NOTIFY_SUCCESS,
            ..Default::default()
        };
        assert!(Recipient::from_smtp(to, &test_cfg()).is_err());
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

    fn domain_string() -> impl Strategy<Value = String> {
        proptest::string::string_regex(
            r#"[a-zA-Z0-9.:\[\]_ \x00-\x1f\x7f\u{80}-\u{9f}é-]{0,20}|\[(IPv6:)?[0-9a-f:.]{0,20}\]"#,
        )
        .unwrap()
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
        fn client_domain_no_panic_and_roundtrips(s in domain_string()) {
            if let Ok(d) = Domain::parse_client(&s) {
                prop_assert_eq!(Domain::parse_client(&d.to_string()), Ok(d.clone()));
                prop_assert!(d.to_string().bytes().all(|b| b.is_ascii_graphic()));
            }
        }

        #[test]
        fn mailbox_domain_no_panic_and_roundtrips(s in domain_string()) {
            if let Ok(d) = Domain::parse(&s) {
                prop_assert_eq!(Domain::parse(&d.to_string()), Ok(d.clone()));
                prop_assert!(Domain::parse_client(&s).is_ok());
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
