//! cloudflare's json for zones and dns records. private to `dns`: nothing
//! outside this module sees a wire field.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Record, mark};
use crate::model::Owner;

#[derive(Deserialize)]
pub(super) struct Zone {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
pub(super) struct LiveRecord {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub proxied: Option<bool>,
    pub ttl: u32,
    #[serde(default)]
    pub priority: Option<u16>,
    #[serde(default)]
    pub comment: Option<String>,
    pub created_on: DateTime<Utc>,
}

/// a create or a whole-object update: every field norn manages, the mark included
#[derive(Serialize, Debug, PartialEq)]
pub(super) struct Body {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxied: Option<bool>,
    pub ttl: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u16>,
    pub comment: String,
}

/// only the mark: what adopting sends
#[derive(Serialize)]
pub(super) struct Mark {
    pub comment: String,
}

pub(super) fn body(record: &Record, owner: &Owner) -> Body {
    Body {
        kind: record.data.record_type().as_str().to_owned(),
        name: record.name.to_string(),
        content: record.data.wire_content(),
        proxied: record.data.reach().map(|reach| reach.proxied()),
        ttl: record.data.ttl().to_wire(),
        priority: record.data.priority(),
        comment: mark::write(owner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dns::Ttl, model::HostName};

    fn web() -> Owner {
        Owner::Host(HostName::parse("web-1").unwrap())
    }

    #[test]
    fn a_proxied_record_is_written_with_the_automatic_ttl() {
        let record =
            Record::declared("example.com", "A", "192.0.2.1", Some(true), Ttl::Auto, None).unwrap();
        let body = body(&record, &web());
        assert_eq!(
            (body.proxied, body.ttl, body.comment.as_str()),
            (Some(true), 1, "norn v1 host=web-1")
        );
    }

    #[test]
    fn only_proxiable_types_carry_proxied_and_only_mx_a_priority() {
        let mx = Record::declared(
            "example.com",
            "MX",
            "mail.example.com",
            None,
            Ttl::Seconds(300),
            Some(10),
        )
        .unwrap();
        let mx = body(&mx, &Owner::Fleet);
        assert_eq!((mx.proxied, mx.priority, mx.ttl), (None, Some(10), 300));
        let txt =
            Record::declared("example.com", "TXT", "v=spf1 -all", None, Ttl::Auto, None).unwrap();
        let json = serde_json::to_value(body(&txt, &Owner::Fleet)).unwrap();
        assert_eq!(json.get("proxied"), None);
        assert_eq!(json["content"], "v=spf1 -all");
    }
}
