use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, TimeDelta, Utc};

use crate::{
    dns::{self, Dns, Record, Reported, Ttl},
    kind::{Live, Ownership, Step, Want},
    model::{HostName, Owner, Zone},
};

pub(crate) const WEB_1: &str = "web-1";
pub(crate) const WEB_2: &str = "web-2";

pub(crate) fn host(name: &str) -> Owner {
    Owner::Host(HostName::parse(name).unwrap())
}

pub(crate) fn zone(name: &str) -> Zone {
    Zone::parse(name).unwrap()
}

static SERIAL: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct Wire {
    pub kind: &'static str,
    pub name: String,
    pub content: String,
    pub proxied: Option<bool>,
    pub ttl: u32,
    pub priority: Option<u16>,
    pub comment: Option<String>,
}

pub(crate) fn wire(kind: &'static str, name: &str, content: &str) -> Wire {
    let proxiable = matches!(kind, "A" | "AAAA" | "CNAME");
    Wire {
        kind,
        name: name.into(),
        content: content.into(),
        proxied: proxiable.then_some(false),
        ttl: 1,
        priority: None,
        comment: None,
    }
}

impl Wire {
    pub fn proxied(mut self) -> Self {
        self.proxied = Some(true);
        self
    }

    pub fn ttl(mut self, ttl: u32) -> Self {
        self.ttl = ttl;
        self
    }

    pub fn priority(mut self, priority: u16) -> Self {
        self.priority = Some(priority);
        self
    }

    pub fn comment(mut self, comment: &str) -> Self {
        self.comment = Some(comment.into());
        self
    }

    pub fn marked(self, owner: &Owner) -> Self {
        self.comment(&dns::mark(owner))
    }

    pub fn live(&self) -> Live<Dns> {
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let created = DateTime::<Utc>::UNIX_EPOCH + TimeDelta::seconds(serial as i64);
        Reported {
            id: &format!("{serial:032x}"),
            created,
            kind: self.kind,
            name: &self.name,
            content: &self.content,
            proxied: self.proxied,
            ttl: self.ttl,
            priority: self.priority,
            comment: self.comment.as_deref(),
        }
        .parse()
        .expect("fixture names parse")
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "type": self.kind,
            "name": self.name,
            "content": self.content,
            "proxiable": self.proxied.is_some(),
            "proxied": self.proxied.unwrap_or(false),
            "ttl": self.ttl,
            "priority": self.priority,
            "comment": self.comment,
            "settings": {},
            "meta": {},
            "tags": [],
        })
    }

    pub fn written(record: &Record, owner: &Owner) -> Self {
        let reach = record.data.reach();
        Wire {
            kind: match record.data.record_type() {
                dns::RecordType::A => "A",
                dns::RecordType::Aaaa => "AAAA",
                dns::RecordType::Cname => "CNAME",
                dns::RecordType::Mx => "MX",
                dns::RecordType::Txt => "TXT",
                dns::RecordType::Other(_) => {
                    unreachable!("norn writes only the types it declares")
                }
            },
            name: record.name.to_string(),
            content: record.data.wire_content(),
            proxied: reach.map(|reach| reach.proxied()),
            ttl: record.data.ttl().to_wire(),
            priority: record.data.priority(),
            comment: Some(dns::mark(owner)),
        }
    }
}

pub(crate) fn want(owner: &Owner, kind: &str, name: &str, content: &str) -> Want<Dns> {
    declare(owner, kind, name, content, None, Ttl::Auto, None)
}

pub(crate) fn proxied(owner: &Owner, kind: &str, name: &str, content: &str) -> Want<Dns> {
    declare(owner, kind, name, content, Some(true), Ttl::Auto, None)
}

pub(crate) fn mx(owner: &Owner, name: &str, exchange: &str, priority: u16) -> Want<Dns> {
    declare(owner, "MX", name, exchange, None, Ttl::Auto, Some(priority))
}

pub(crate) fn declare(
    owner: &Owner,
    kind: &str,
    name: &str,
    content: &str,
    proxied: Option<bool>,
    ttl: Ttl,
    priority: Option<u16>,
) -> Want<Dns> {
    let value = Record::declared(name, kind, content, proxied, ttl, priority).unwrap();
    let zone = ["example.com", "example.net", "example.org"]
        .into_iter()
        .map(zone)
        .find(|zone| value.name.is_within(zone))
        .expect("fixture names are within the fixture zones");
    Want {
        scope: zone,
        owner: owner.clone(),
        value,
    }
}

pub(crate) fn apply(live: Vec<Live<Dns>>, steps: &[Step<Dns>]) -> Vec<Live<Dns>> {
    let mut live = live;
    for step in steps {
        match step {
            Step::Adopt {
                owner,
                live: object,
            } => {
                let found = live.iter_mut().find(|l| l.handle == object.handle).unwrap();
                found.ownership = Ownership::Marked(owner.clone());
            }
            Step::Update {
                owner,
                live: object,
                want,
            } => {
                let found = live.iter_mut().find(|l| l.handle == object.handle).unwrap();
                let mut rewritten = Wire::written(want, owner).live();
                rewritten.handle = found.handle.clone();
                rewritten.born = found.born.clone();
                *found = rewritten;
            }
            Step::Delete { live: object, .. } => live.retain(|l| l.handle != object.handle),
            Step::Create { owner, want } => live.push(Wire::written(want, owner).live()),
        }
    }
    live
}

pub(crate) fn baseline() -> Vec<(Zone, Vec<Live<Dns>>)> {
    baseline_records()
        .into_iter()
        .map(|(name, records)| (zone(name), records.iter().map(Wire::live).collect()))
        .collect()
}

pub(crate) fn baseline_records() -> Vec<(&'static str, Vec<Wire>)> {
    let net = vec![
        wire("A", "blog.example.net", "192.0.2.20").proxied(),
        wire("A", "example.net", "192.0.2.20").proxied(),
        wire("CNAME", "www.example.net", "example.net").proxied(),
    ];
    let com = vec![
        wire("A", "blog.example.com", "192.0.2.20").proxied(),
        wire("A", "cache.example.com", "192.0.2.10"),
        wire("A", "derp.example.com", "192.0.2.20"),
        wire("A", "dns.example.com", "192.0.2.20"),
        wire("A", "files.example.com", "192.0.2.20").proxied(),
        wire("A", "heimdall.example.com", "192.0.2.10").proxied(),
        wire("A", "mail.example.com", "192.0.2.20"),
        wire("A", "minecraft.example.com", "192.0.2.20"),
        wire("A", "example.com", "192.0.2.20").proxied(),
        wire("A", "realms.example.com", "192.0.2.20"),
        wire("A", "s3.cache.example.com", "192.0.2.10"),
        wire("A", "status.example.com", "192.0.2.1")
            .proxied()
            .comment("Dummy IP for redirect rule to work"),
        wire("A", "tailscale.example.com", "192.0.2.20").proxied(),
        wire("CNAME", "www.example.com", "example.com").proxied(),
        wire("MX", "example.com", "mail.example.com").priority(10),
        wire(
            "TXT",
            "_acme-challenge.example.com",
            "\"Zq1mP0aJx4sV8tN2cR6yB3uL9wE5kH7dG0fA2oI4\"",
        )
        .ttl(120),
        wire(
            "TXT",
            "_acme-challenge.www.example.com",
            "\"yB3uL9wE5kH7dG0fA2oI4Zq1mP0aJx4sV8tN2cR6\"",
        )
        .ttl(120),
        wire(
            "TXT",
            "_dmarc.example.com",
            "\"v=DMARC1; p=none; rua=mailto:rua@example.net\"",
        ),
        wire(
            "TXT",
            "mail._domainkey.example.com",
            &format!("\"k=rsa;p={}\"", dkim()),
        ),
        wire(
            "TXT",
            "example.com",
            "\"verification-code:0123456789abcdef\"",
        ),
        wire("TXT", "example.com", "v=spf1 include:spf.example.net ~all"),
    ];
    let org = vec![
        wire("A", "example.org", "192.0.2.30"),
        wire("CNAME", "www.example.org", "example.org").proxied(),
        wire("MX", "example.org", "route3.mx.example.net").priority(50),
        wire("MX", "example.org", "route2.mx.example.net").priority(98),
        wire("MX", "example.org", "route1.mx.example.net").priority(46),
        wire(
            "TXT",
            "cf2024-1._domainkey.example.org",
            &format!("\"v=DKIM1; h=sha256; k=rsa; p={}\"", dkim()),
        ),
        wire(
            "TXT",
            "_dmarc.example.org",
            "\"v=DMARC1; p=none; rua=mailto:dmarc@example.net\"",
        ),
        wire(
            "TXT",
            "example.org",
            "\"v=spf1 include:_spf.mx.example.net ~all\"",
        ),
    ];
    vec![
        ("example.com", com),
        ("example.net", net),
        ("example.org", org),
    ]
}

pub(crate) fn dkim() -> String {
    "MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQC".repeat(5)
}

pub(crate) fn web_1() -> Vec<Want<Dns>> {
    let me = host(WEB_1);
    vec![
        want(&me, "A", "cache.example.com", "192.0.2.10"),
        want(&me, "A", "s3.cache.example.com", "192.0.2.10"),
        proxied(&me, "A", "heimdall.example.com", "192.0.2.10"),
        want(&me, "A", "example.org", "192.0.2.30"),
        proxied(&me, "CNAME", "www.example.org", "example.org"),
    ]
}

pub(crate) fn web_2() -> Vec<Want<Dns>> {
    let me = host(WEB_2);
    let mut wants = vec![
        proxied(&me, "A", "example.net", "192.0.2.20"),
        proxied(&me, "A", "blog.example.net", "192.0.2.20"),
        proxied(&me, "CNAME", "www.example.net", "example.net"),
        proxied(&me, "CNAME", "www.example.com", "example.com"),
        mx(&me, "example.com", "mail.example.com", 10),
        want(
            &me,
            "TXT",
            "example.com",
            "v=spf1 include:spf.example.net ~all",
        ),
        want(
            &me,
            "TXT",
            "example.com",
            "verification-code:0123456789abcdef",
        ),
        want(
            &me,
            "TXT",
            "_dmarc.example.com",
            "v=DMARC1; p=none; rua=mailto:rua@example.net",
        ),
        want(
            &me,
            "TXT",
            "mail._domainkey.example.com",
            &format!("k=rsa;p={}", dkim()),
        ),
    ];
    for name in [
        "example.com",
        "blog.example.com",
        "files.example.com",
        "tailscale.example.com",
    ] {
        wants.push(proxied(&me, "A", name, "192.0.2.20"));
    }
    for name in ["dns", "derp", "mail", "minecraft", "realms"] {
        wants.push(want(&me, "A", &format!("{name}.example.com"), "192.0.2.20"));
    }
    wants
}
