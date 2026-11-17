use std::{
    fmt,
    net::{Ipv4Addr, Ipv6Addr},
};

use crate::model::Fqdn;

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Record {
    pub name: Fqdn,
    pub data: Rdata,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Rdata {
    A {
        address: Ipv4Addr,
        reach: Reach,
    },
    Aaaa {
        address: Ipv6Addr,
        reach: Reach,
    },
    Cname {
        target: Fqdn,
        reach: Reach,
    },
    Mx {
        exchange: Fqdn,
        priority: u16,
        ttl: Ttl,
    },
    Txt {
        text: Text,
        ttl: Ttl,
    },
}

/// cloudflare answers a proxied record with an automatic ttl whatever ttl it
/// was sent, so a proxied reach carries none
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Reach {
    Proxied,
    Direct(Ttl),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ttl {
    Auto,
    Seconds(u32),
}

impl Ttl {
    pub fn declared(seconds: u32) -> Result<Self, String> {
        if (60..=86400).contains(&seconds) {
            Ok(Ttl::Seconds(seconds))
        } else {
            Err(format!(
                "ttl {seconds} is outside 60 to 86400 seconds; leave it out for auto"
            ))
        }
    }

    pub fn from_wire(ttl: u32) -> Self {
        if ttl == 1 {
            Ttl::Auto
        } else {
            Ttl::Seconds(ttl)
        }
    }

    pub fn to_wire(self) -> u32 {
        match self {
            Ttl::Auto => 1,
            Ttl::Seconds(seconds) => seconds,
        }
    }
}

impl Reach {
    pub fn proxied(self) -> bool {
        self == Reach::Proxied
    }

    pub fn ttl(self) -> Ttl {
        match self {
            Reach::Proxied => Ttl::Auto,
            Reach::Direct(ttl) => ttl,
        }
    }
}

/// a TXT record's text as resolvers join it. cloudflare reports `ab`, `"ab"`
/// and `"a" "b"` for texts that compare equal here, so the text is kept
/// decoded and only split into quoted 255-byte strings when written.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Text(String);

impl Text {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() || text.len() > 2048 {
            return Err(format!(
                "a text is 1 to 2048 bytes long, not {}",
                text.len()
            ));
        }
        if text.chars().any(char::is_control) {
            return Err("a text cannot hold control characters".into());
        }
        Ok(Self(text.to_owned()))
    }

    pub fn from_live(content: &str) -> Self {
        Self(unquote(content).unwrap_or_else(|| content.to_owned()))
    }

    pub fn to_wire(&self) -> String {
        if self.0.len() <= 255 && !self.0.starts_with('"') {
            return self.0.clone();
        }
        let mut chunks = Vec::new();
        let mut rest = self.0.as_str();
        while !rest.is_empty() {
            let mut end = rest.len().min(255);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            let (chunk, tail) = rest.split_at(end);
            chunks.push(format!(
                "\"{}\"",
                chunk.replace('\\', "\\\\").replace('"', "\\\"")
            ));
            rest = tail;
        }
        chunks.join(" ")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn unquote(content: &str) -> Option<String> {
    let mut text = String::new();
    let mut chars = content.trim().chars().peekable();
    chars.peek()?;
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c != '"' {
            return None;
        }
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => text.push(chars.next()?),
                c => text.push(c),
            }
        }
    }
    Some(text)
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Content {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    Name(Fqdn),
    Text(Text),
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) enum RecordType {
    A,
    Aaaa,
    Cname,
    Mx,
    Txt,
    Other(String),
}

impl RecordType {
    pub fn parse(kind: &str) -> Self {
        match kind {
            "A" => RecordType::A,
            "AAAA" => RecordType::Aaaa,
            "CNAME" => RecordType::Cname,
            "MX" => RecordType::Mx,
            "TXT" => RecordType::Txt,
            other => RecordType::Other(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            RecordType::A => "A",
            RecordType::Aaaa => "AAAA",
            RecordType::Cname => "CNAME",
            RecordType::Mx => "MX",
            RecordType::Txt => "TXT",
            RecordType::Other(other) => other,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Slot {
    pub name: Fqdn,
    pub kind: RecordType,
}

impl Record {
    pub fn slot(&self) -> Slot {
        Slot {
            name: self.name.clone(),
            kind: self.data.record_type(),
        }
    }

    pub fn declared(
        name: &str,
        kind: &str,
        content: &str,
        proxied: Option<bool>,
        ttl: Ttl,
        priority: Option<u16>,
    ) -> Result<Self, String> {
        let name = Fqdn::parse(name)?;
        let kind = RecordType::parse(kind);
        let proxiable = matches!(kind, RecordType::A | RecordType::Aaaa | RecordType::Cname);
        if proxied.is_some() && !proxiable {
            return Err(format!("{} records cannot be proxied", kind.as_str()));
        }
        if priority.is_some() != (kind == RecordType::Mx) {
            return Err("an MX record has a priority, and no other type does".into());
        }
        let reach = match (proxied, ttl) {
            (Some(true), Ttl::Auto) => Reach::Proxied,
            (Some(true), Ttl::Seconds(_)) => {
                return Err("a proxied record's ttl is always auto".into());
            }
            (_, ttl) => Reach::Direct(ttl),
        };
        let address = |what: &str| match content {
            "" => {
                format!("no {what} address: list the record's addresses, or set cloudflare.address")
            }
            _ => format!("{content:?} is not an {what} address"),
        };
        let data = match kind {
            RecordType::A => Rdata::A {
                address: content.parse().map_err(|_| address("IPv4"))?,
                reach,
            },
            RecordType::Aaaa => Rdata::Aaaa {
                address: content.parse().map_err(|_| address("IPv6"))?,
                reach,
            },
            RecordType::Cname => Rdata::Cname {
                target: Fqdn::parse(content)?,
                reach,
            },
            RecordType::Mx => Rdata::Mx {
                exchange: Fqdn::parse(content)?,
                priority: priority.unwrap_or_default(),
                ttl,
            },
            RecordType::Txt => Rdata::Txt {
                text: Text::parse(content)?,
                ttl,
            },
            RecordType::Other(other) => {
                return Err(format!(
                    "norn declares A, AAAA, CNAME, MX and TXT records, not {other}"
                ));
            }
        };
        Ok(Self { name, data })
    }

    pub fn live(
        name: &Fqdn,
        kind: &RecordType,
        content: &str,
        proxied: Option<bool>,
        ttl: u32,
        priority: Option<u16>,
    ) -> Option<Self> {
        let ttl = Ttl::from_wire(ttl);
        let reach = if proxied == Some(true) {
            Reach::Proxied
        } else {
            Reach::Direct(ttl)
        };
        let data = match kind {
            RecordType::A => Rdata::A {
                address: content.parse().ok()?,
                reach,
            },
            RecordType::Aaaa => Rdata::Aaaa {
                address: content.parse().ok()?,
                reach,
            },
            RecordType::Cname => Rdata::Cname {
                target: Fqdn::from_live(content)?,
                reach,
            },
            RecordType::Mx => Rdata::Mx {
                exchange: Fqdn::from_live(content)?,
                priority: priority?,
                ttl,
            },
            RecordType::Txt => Rdata::Txt {
                text: Text::from_live(content),
                ttl,
            },
            RecordType::Other(_) => return None,
        };
        Some(Self {
            name: name.clone(),
            data,
        })
    }
}

impl Rdata {
    pub fn content(&self) -> Content {
        match self {
            Rdata::A { address, .. } => Content::V4(*address),
            Rdata::Aaaa { address, .. } => Content::V6(*address),
            Rdata::Cname { target, .. } => Content::Name(target.clone()),
            Rdata::Mx { exchange, .. } => Content::Name(exchange.clone()),
            Rdata::Txt { text, .. } => Content::Text(text.clone()),
        }
    }

    pub fn record_type(&self) -> RecordType {
        match self {
            Rdata::A { .. } => RecordType::A,
            Rdata::Aaaa { .. } => RecordType::Aaaa,
            Rdata::Cname { .. } => RecordType::Cname,
            Rdata::Mx { .. } => RecordType::Mx,
            Rdata::Txt { .. } => RecordType::Txt,
        }
    }

    pub fn wire_content(&self) -> String {
        match self {
            Rdata::A { address, .. } => address.to_string(),
            Rdata::Aaaa { address, .. } => address.to_string(),
            Rdata::Cname { target, .. } => target.to_string(),
            Rdata::Mx { exchange, .. } => exchange.to_string(),
            Rdata::Txt { text, .. } => text.to_wire(),
        }
    }

    pub fn reach(&self) -> Option<Reach> {
        match self {
            Rdata::A { reach, .. } | Rdata::Aaaa { reach, .. } | Rdata::Cname { reach, .. } => {
                Some(*reach)
            }
            Rdata::Mx { .. } | Rdata::Txt { .. } => None,
        }
    }

    pub fn ttl(&self) -> Ttl {
        match self {
            Rdata::A { reach, .. } | Rdata::Aaaa { reach, .. } | Rdata::Cname { reach, .. } => {
                reach.ttl()
            }
            Rdata::Mx { ttl, .. } | Rdata::Txt { ttl, .. } => *ttl,
        }
    }

    pub fn priority(&self) -> Option<u16> {
        match self {
            Rdata::Mx { priority, .. } => Some(*priority),
            _ => None,
        }
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:<6} {}", self.kind.as_str(), self.name)
    }
}

impl fmt::Display for Ttl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ttl::Auto => f.write_str("auto"),
            Ttl::Seconds(seconds) => write!(f, "{seconds}"),
        }
    }
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ttl = |ttl: Ttl| match ttl {
            Ttl::Auto => String::new(),
            Ttl::Seconds(seconds) => format!("  ttl {seconds}"),
        };
        let reach = |reach: Reach| match reach {
            Reach::Proxied => "  proxied".to_owned(),
            Reach::Direct(t) => format!("  dns only{}", ttl(t)),
        };
        match &self.data {
            Rdata::A { address, reach: r } => write!(f, "{address}{}", reach(*r)),
            Rdata::Aaaa { address, reach: r } => write!(f, "{address}{}", reach(*r)),
            Rdata::Cname { target, reach: r } => write!(f, "{target}{}", reach(*r)),
            Rdata::Mx {
                exchange,
                priority,
                ttl: t,
            } => write!(f, "{priority} {exchange}{}", ttl(*t)),
            Rdata::Txt { text, ttl: t } => write!(f, "{:?}{}", text.as_str(), ttl(*t)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(name: &str) -> Fqdn {
        Fqdn::parse(name).unwrap()
    }

    #[test]
    fn quoted_and_bare_txt_with_one_text_are_equal() {
        let declared =
            Record::declared("example.com", "TXT", "v=spf1 -all", None, Ttl::Auto, None).unwrap();
        for content in [
            "v=spf1 -all",
            "\"v=spf1 -all\"",
            "\"v=spf1\" \" -all\"",
            "\"v=spf1 \"\"-all\"",
        ] {
            let live = Record::live(
                &name("example.com"),
                &RecordType::Txt,
                content,
                Some(false),
                1,
                None,
            );
            assert_eq!(live.as_ref(), Some(&declared), "{content}");
        }
    }

    #[test]
    fn a_long_text_is_written_in_quoted_chunks_that_read_back_equal() {
        let long = format!("v=DKIM1; k=rsa; p={}\"x", "A".repeat(400));
        let text = Text::parse(&long).unwrap();
        let wire = text.to_wire();
        assert!(wire.starts_with('"') && wire.contains("\" \""));
        assert_eq!(Text::from_live(&wire), text);
        assert_eq!(
            Text::from_live(&Text::parse("\"lead").unwrap().to_wire()).as_str(),
            "\"lead"
        );
    }

    #[test]
    fn a_proxied_record_reported_with_ttl_1_equals_a_proxied_declaration() {
        let declared = Record::declared(
            "example.com",
            "A",
            "192.0.2.10",
            Some(true),
            Ttl::Auto,
            None,
        )
        .unwrap();
        let live = Record::live(
            &name("example.com"),
            &RecordType::A,
            "192.0.2.10",
            Some(true),
            1,
            None,
        );
        assert_eq!(live, Some(declared));
    }

    #[test]
    fn a_proxied_record_with_a_ttl_cannot_be_declared() {
        let declared = Record::declared(
            "example.com",
            "A",
            "192.0.2.10",
            Some(true),
            Ttl::Seconds(300),
            None,
        );
        assert!(declared.unwrap_err().contains("proxied"));
    }

    #[test]
    fn a_declaration_holds_exactly_its_type_fields() {
        assert!(Record::declared("example.com", "TXT", "x", Some(false), Ttl::Auto, None).is_err());
        assert!(
            Record::declared(
                "example.com",
                "MX",
                "mail.example.com",
                None,
                Ttl::Auto,
                None
            )
            .is_err()
        );
        assert!(
            Record::declared("example.com", "A", "192.0.2.1", None, Ttl::Auto, Some(10)).is_err()
        );
        assert!(Record::declared("example.com", "SRV", "x", None, Ttl::Auto, None).is_err());
        assert!(
            Record::declared("example.com", "A", "2001:db8::1", None, Ttl::Auto, None).is_err()
        );
    }

    #[test]
    fn a_type_norn_does_not_model_has_no_value() {
        let live = Record::live(
            &name("example.com"),
            &RecordType::parse("SRV"),
            "0 5 5060 sip.example.com",
            None,
            1,
            None,
        );
        assert_eq!(live, None);
    }
}
