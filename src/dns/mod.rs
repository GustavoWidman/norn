mod mark;
mod value;
mod wire;

pub(crate) use value::{Content, Rdata, Record, RecordType, Slot, Ttl};

use chrono::{DateTime, Utc};

use crate::{
    cloudflare::{Api, ApiError, Capability, Verb, Write},
    kind::{Born, Kind, Live, Step},
    model::{Fqdn, Zone},
};

#[derive(Clone, Debug)]
pub(crate) struct Dns;

#[derive(Clone, Debug)]
pub(crate) struct ZoneId(String);

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct RecordId(pub String);

impl Kind for Dns {
    const NAME: &'static str = "dns record";

    type Scope = Zone;
    type Place = ZoneId;
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

    fn read<C: Capability>(
        api: &Api<C>,
        zone: &Zone,
    ) -> Result<(ZoneId, Vec<Live<Dns>>), ApiError> {
        let zones: Vec<wire::Zone> = api.get("/zones", &[("name", zone.name().as_str())])?;
        let id = zones
            .into_iter()
            .find(|found| found.name == zone.name().as_str())
            .ok_or_else(|| ApiError::Missing(format!("zone {zone}")))?
            .id;
        let records: Vec<wire::LiveRecord> = api.list(&format!("/zones/{id}/dns_records"), &[])?;
        let live = records
            .iter()
            .filter_map(|record| {
                let reported = Reported {
                    id: &record.id,
                    created: record.created_on,
                    kind: &record.kind,
                    name: &record.name,
                    content: &record.content,
                    proxied: record.proxied,
                    ttl: record.ttl,
                    priority: record.priority,
                    comment: record.comment.as_deref(),
                };
                let parsed = reported.parse();
                if parsed.is_none() {
                    log::warn!(
                        "{zone}: skipping {} {:?}, a name norn cannot spell",
                        record.kind,
                        record.name
                    );
                }
                parsed
            })
            .collect();
        Ok((ZoneId(id), live))
    }

    fn write(api: &Api<Write>, zone: &ZoneId, step: &Step<Dns>) -> Result<(), ApiError> {
        let ZoneId(zone) = zone;
        let at = |live: &Live<Dns>| format!("/zones/{zone}/dns_records/{}", live.handle.0);
        let sent: Result<serde_json::Value, ApiError> = match step {
            Step::Adopt { owner, live } => api.send(
                Verb::Patch,
                &at(live),
                Some(&wire::Mark {
                    comment: mark::write(owner),
                }),
            ),
            Step::Update { owner, live, want } => {
                api.send(Verb::Patch, &at(live), Some(&wire::body(want, owner)))
            }
            Step::Delete { live, .. } => api.send(Verb::Delete, &at(live), None::<&()>),
            Step::Create { owner, want } => api.send(
                Verb::Post,
                &format!("/zones/{zone}/dns_records"),
                Some(&wire::body(want, owner)),
            ),
        };
        match (sent, step) {
            (Ok(_), _) => Ok(()),
            (Err(ApiError::NotFound), Step::Delete { .. }) => Ok(()),
            (Err(error), _) => Err(error),
        }
    }
}

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

#[cfg(test)]
pub(crate) fn mark(owner: &crate::model::HostName) -> String {
    mark::write(owner)
}
