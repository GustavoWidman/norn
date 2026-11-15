//! dns records: the first kind. everything cloudflare-specific about a
//! record lives here: its values, its mark, its wire shape.

mod mark;
mod value;

#[cfg(test)]
pub(crate) use value::Ttl;
pub(crate) use value::{Content, Record, RecordType, Slot};

use chrono::{DateTime, Utc};

use crate::{
    kind::{Born, Kind, Live},
    model::{Fqdn, Zone},
};

#[derive(Clone, Debug)]
pub(crate) struct Dns;

/// a record as cloudflare addresses it
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct RecordId(pub String);

impl Kind for Dns {
    const NAME: &'static str = "dns record";

    type Scope = Zone;
    type Key = Slot;
    type Identity = Content;
    type Value = Record;
    type Handle = RecordId;

    fn key(record: &Record) -> Slot {
        record.slot()
    }

    fn identity(record: &Record) -> Content {
        record.data.content()
    }

    fn excludes(want: &Slot, other: &Slot) -> bool {
        want.name == other.name
            && (want.kind == RecordType::Cname || other.kind == RecordType::Cname)
    }
}

/// the fields of a record cloudflare reports, as norn reads them
pub(crate) struct Reported<'a> {
    pub id: &'a str,
    pub created: DateTime<Utc>,
    pub kind: &'a str,
    pub name: &'a str,
    pub content: &'a str,
    pub proxied: Option<bool>,
    pub ttl: u32,
    pub priority: Option<u16>,
    pub comment: Option<&'a str>,
}

impl Reported<'_> {
    /// `None` for a name norn cannot spell: it can be nobody's declaration
    pub fn parse(&self) -> Option<Live<Dns>> {
        let name = Fqdn::from_live(self.name)?;
        let kind = RecordType::parse(self.kind);
        Some(Live {
            handle: RecordId(self.id.to_owned()),
            value: Record::live(
                &name,
                &kind,
                self.content,
                self.proxied,
                self.ttl,
                self.priority,
            ),
            key: Slot { name, kind },
            ownership: mark::read(self.comment),
            born: Born {
                created: self.created,
                id: self.id.to_owned(),
            },
        })
    }
}

/// the comment that marks `owner`'s records
pub(crate) fn mark(owner: &crate::model::Owner) -> String {
    mark::write(owner)
}
