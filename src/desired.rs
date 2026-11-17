use std::{collections::BTreeMap, fmt, path::Path};

use crate::{
    dns::{Dns, Record, RecordType, Slot},
    kind::{Ladder, Want},
    model::{Fqdn, HostName, Owner, Owners, Zones},
};

const SCHEMA: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Report,
    Write(Ladder),
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Report => "report",
            Mode::Write(Ladder::Adopt) => "adopt",
            Mode::Write(Ladder::Enforce) => "enforce",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Desired {
    pub host: HostName,
    pub mode: Mode,
    pub zones: Zones,
    pub host_owned: Vec<Record>,
    pub fleet_owned: Vec<Record>,
}

pub(crate) struct Wants {
    pub dns: Vec<Want<Dns>>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Problem {
    pub subject: String,
    pub message: String,
}

#[derive(Debug)]
pub struct Problems(pub(crate) Vec<Problem>);

impl fmt::Display for Problems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.0.len();
        write!(
            f,
            "norn: the cloudflare declarations have {count} problem{}",
            if count == 1 { "" } else { "s" }
        )?;
        for problem in &self.0 {
            write!(f, "\n  {}: {}", problem.subject, problem.message)?;
        }
        Ok(())
    }
}

impl Desired {
    pub fn read(path: &Path) -> Result<Self, Problems> {
        let bytes = std::fs::read(path).map_err(|error| {
            Problems(vec![Problem {
                subject: path.display().to_string(),
                message: error.to_string(),
            }])
        })?;
        Self::parse(&bytes)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, Problems> {
        let document: document::Document = serde_json::from_slice(bytes).map_err(|error| {
            Problems(vec![Problem {
                subject: "the desired file".into(),
                message: error.to_string(),
            }])
        })?;

        let mut problems = Vec::new();
        let mut problem = |subject: &str, message: String| {
            problems.push(Problem {
                subject: subject.to_owned(),
                message,
            });
        };

        if document.schema != SCHEMA {
            problem(
                "schema",
                format!("this norn reads schema {SCHEMA}, not {}", document.schema),
            );
        }
        let host = HostName::parse(&document.host)
            .map_err(|message| problem("host", message))
            .ok();
        let mode = match document.mode.as_str() {
            "report" => Some(Mode::Report),
            "adopt" => Some(Mode::Write(Ladder::Adopt)),
            "enforce" => Some(Mode::Write(Ladder::Enforce)),
            other => {
                problem("mode", format!("{other:?} is not report, adopt or enforce"));
                None
            }
        };
        let zones = Zones::parse(&document.zones)
            .map_err(|messages| {
                messages
                    .into_iter()
                    .for_each(|message| problem("zones", message))
            })
            .ok();

        let mut host_owned = Vec::new();
        let mut fleet_owned = Vec::new();
        for (records, owned, into) in [
            (&document.host_owned.dns, "host", &mut host_owned),
            (&document.fleet_owned.dns, "fleet", &mut fleet_owned),
        ] {
            for declared in records {
                let subject = format!("{owned} dns {} {}", declared.name, declared.kind);
                let record = declared.ttl.parse().and_then(|ttl| {
                    Record::declared(
                        &declared.name,
                        &declared.kind,
                        &declared.content,
                        declared.proxied,
                        ttl,
                        declared.priority,
                    )
                });
                match (record, &zones) {
                    (Err(message), _) => problem(&subject, message),
                    (Ok(record), Some(zones)) if zones.zone_of(&record.name).is_none() => {
                        problem(
                            &subject,
                            format!("{} is outside every managed zone ({zones})", record.name),
                        );
                    }
                    (Ok(record), _) => into.push(record),
                }
            }
        }

        for records in [&mut host_owned, &mut fleet_owned] {
            records.sort_by_key(|record| (record.slot(), record.data.content()));
        }
        for (subject, message) in collisions(&host_owned, &fleet_owned) {
            problem(&subject, message);
        }

        match (host, mode, zones) {
            (Some(host), Some(mode), Some(zones)) if problems.is_empty() => Ok(Self {
                host,
                mode,
                zones,
                host_owned,
                fleet_owned,
            }),
            _ => Err(Problems(problems)),
        }
    }

    pub fn canonical(&self) -> String {
        let section = |records: &[Record]| document::Objects {
            dns: records.iter().map(document::Record::from).collect(),
        };
        let document = document::Document {
            schema: SCHEMA,
            host: self.host.to_string(),
            mode: self.mode.as_str().to_owned(),
            zones: self.zones.iter().map(ToString::to_string).collect(),
            host_owned: section(&self.host_owned),
            fleet_owned: section(&self.fleet_owned),
        };
        let mut text = serde_json::to_string_pretty(&document).expect("the document serializes");
        text.push('\n');
        text
    }

    pub fn owners(&self) -> Owners {
        Owners::machine(self.host.clone())
    }

    pub fn wants(&self) -> Wants {
        Wants { dns: self.dns() }
    }

    fn dns(&self) -> Vec<Want<Dns>> {
        let owned = |owner: Owner, records: &[Record]| -> Vec<Want<Dns>> {
            records
                .iter()
                .map(|record| Want {
                    scope: self
                        .zones
                        .zone_of(&record.name)
                        .expect("parse checked every name's zone")
                        .clone(),
                    owner: owner.clone(),
                    value: record.clone(),
                })
                .collect()
        };
        let mut wants = owned(Owner::Host(self.host.clone()), &self.host_owned);
        wants.extend(owned(Owner::Fleet, &self.fleet_owned));
        wants
    }
}

fn collisions(host_owned: &[Record], fleet_owned: &[Record]) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut slots: BTreeMap<Slot, Vec<(&str, &Record)>> = BTreeMap::new();
    for (owner, records) in [("host", host_owned), ("fleet", fleet_owned)] {
        for record in records {
            slots
                .entry(record.slot())
                .or_default()
                .push((owner, record));
        }
    }
    for (slot, records) in &slots {
        let subject = format!("dns {} {}", slot.name, slot.kind.as_str());
        if records.iter().any(|(owner, _)| *owner == "host")
            && records.iter().any(|(owner, _)| *owner == "fleet")
        {
            found.push((
                subject.clone(),
                "declared as both this machine's and the fleet's; one name and type has one owner"
                    .into(),
            ));
        }
        let mut contents: Vec<_> = records
            .iter()
            .map(|(_, record)| record.data.content())
            .collect();
        contents.sort();
        if contents.windows(2).any(|pair| pair[0] == pair[1]) {
            found.push((subject.clone(), "the same record is declared twice".into()));
        }
        if slot.kind == RecordType::Cname && records.len() > 1 {
            found.push((subject, "a name holds one CNAME".into()));
        }
    }
    let mut names: BTreeMap<&Fqdn, Vec<&str>> = BTreeMap::new();
    for slot in slots.keys() {
        names
            .entry(&slot.name)
            .or_default()
            .push(slot.kind.as_str());
    }
    for (name, kinds) in names {
        let others: Vec<&str> = kinds
            .iter()
            .copied()
            .filter(|kind| *kind != "CNAME")
            .collect();
        if kinds.contains(&"CNAME") && !others.is_empty() {
            found.push((
                format!("dns {name} CNAME"),
                format!(
                    "a CNAME shares its name with nothing, and {name} also declares {}",
                    others.join(", ")
                ),
            ));
        }
    }
    found
}

mod document {
    use serde::{Deserialize, Serialize};

    use crate::dns::{Rdata, Record as Value, Ttl};

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct Document {
        pub schema: u32,
        pub host: String,
        pub mode: String,
        pub zones: Vec<String>,
        pub host_owned: Objects,
        pub fleet_owned: Objects,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct Objects {
        pub dns: Vec<Record>,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct Record {
        pub name: String,
        #[serde(rename = "type")]
        pub kind: String,
        pub content: String,
        pub proxied: Option<bool>,
        pub ttl: DeclaredTtl,
        pub priority: Option<u16>,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(untagged)]
    pub enum DeclaredTtl {
        Auto(Auto),
        Seconds(u32),
    }

    #[derive(Deserialize, Serialize)]
    #[serde(rename_all = "lowercase")]
    pub enum Auto {
        Auto,
    }

    impl DeclaredTtl {
        pub fn parse(&self) -> Result<Ttl, String> {
            match self {
                DeclaredTtl::Auto(Auto::Auto) => Ok(Ttl::Auto),
                DeclaredTtl::Seconds(seconds) => Ttl::declared(*seconds),
            }
        }
    }

    impl From<&Value> for Record {
        fn from(value: &Value) -> Self {
            let content = match &value.data {
                Rdata::Txt { text, .. } => text.as_str().to_owned(),
                data => data.wire_content(),
            };
            Record {
                name: value.name.to_string(),
                kind: value.data.record_type().as_str().to_owned(),
                content,
                proxied: value.data.reach().map(|reach| reach.proxied()),
                ttl: match value.data.ttl() {
                    Ttl::Auto => DeclaredTtl::Auto(Auto::Auto),
                    Ttl::Seconds(seconds) => DeclaredTtl::Seconds(seconds),
                },
                priority: value.data.priority(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn document(
        host_owned: serde_json::Value,
        fleet_owned: serde_json::Value,
    ) -> serde_json::Value {
        json!({
            "schema": 1,
            "host": "web-1",
            "mode": "report",
            "zones": ["example.com", "example.org"],
            "host_owned": { "dns": host_owned },
            "fleet_owned": { "dns": fleet_owned },
        })
    }

    fn record(name: &str, kind: &str, content: &str) -> serde_json::Value {
        let proxied = matches!(kind, "A" | "AAAA" | "CNAME").then_some(false);
        let priority = (kind == "MX").then_some(10);
        json!({ "name": name, "type": kind, "content": content, "proxied": proxied, "ttl": "auto", "priority": priority })
    }

    fn problems(document: serde_json::Value) -> Vec<String> {
        match Desired::parse(&serde_json::to_vec(&document).unwrap()) {
            Ok(_) => vec![],
            Err(Problems(problems)) => problems
                .into_iter()
                .map(|p| format!("{}: {}", p.subject, p.message))
                .collect(),
        }
    }

    #[test]
    fn the_canonical_form_parses_back_to_itself() {
        let mut long = record(
            "mail._domainkey.example.com",
            "TXT",
            &format!("k=rsa; p={}", "A".repeat(400)),
        );
        long["ttl"] = json!(300);
        let mut proxied = record("example.com", "A", "192.0.2.1");
        proxied["proxied"] = json!(true);
        let document = document(
            json!([
                record("www.example.com", "CNAME", "example.com"),
                long,
                proxied,
                record("example.com", "MX", "mail.example.com"),
                record("example.com", "TXT", "\"quoted\" text"),
            ]),
            json!([record("status.example.org", "A", "192.0.2.9")]),
        );
        let desired = Desired::parse(&serde_json::to_vec(&document).unwrap()).unwrap();
        let canonical = desired.canonical();
        let again = Desired::parse(canonical.as_bytes()).unwrap();
        assert_eq!(again, desired);
        assert_eq!(again.canonical(), canonical);
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let found = problems(document(
            json!([
                record("blog.example.net", "A", "192.0.2.1"),
                record("Blog.example.com", "A", "192.0.2.1"),
                record("api.example.com", "A", "192.0.2.300"),
                record("x.example.com", "SRV", "0 5 5060 sip.example.com"),
            ]),
            json!([]),
        ));
        assert_eq!(found.len(), 4, "{found:#?}");
        assert!(found[0].contains("outside every managed zone (example.com, example.org)"));
        assert!(found[1].contains("canonical"));
        assert!(found[2].contains("not an IPv4 address"));
        assert!(found[3].contains("not SRV"));
    }

    #[test]
    fn one_name_and_type_has_one_owner_and_a_cname_stands_alone() {
        let found = problems(document(
            json!([
                record("app.example.com", "A", "192.0.2.1"),
                record("app.example.com", "A", "192.0.2.1"),
                record("www.example.com", "CNAME", "example.com"),
                record("www.example.com", "TXT", "hello"),
            ]),
            json!([record("app.example.com", "A", "192.0.2.2")]),
        ));
        assert_eq!(found.len(), 3, "{found:#?}");
        assert!(found[0].contains("both this machine's and the fleet's"));
        assert!(found[1].contains("declared twice"));
        assert!(found[2].contains("CNAME shares its name with nothing"));
    }

    #[test]
    fn a_file_of_an_older_schema_is_refused() {
        let found = problems(json!({
            "schema": 1,
            "host": "web-1",
            "mode": "report",
            "zones": ["example.com"],
            "host_owned": { "dns": [] },
            "fleet_owned": { "dns": [] },
        }));
        assert_eq!(
            found,
            [
                "schema: this norn reads schema 2, not 1; rebuild the desired file with this norn's module"
            ]
        );
    }

    #[test]
    fn an_unknown_field_fails() {
        let mut document = document(json!([]), json!([]));
        document["note"] = json!("x");
        assert!(problems(document)[0].contains("unknown field"));
        let mut extra = record("example.com", "A", "192.0.2.1");
        extra["comment"] = json!("x");
        assert!(
            problems(super::tests::document(json!([extra]), json!([])))[0]
                .contains("unknown field")
        );
    }

    #[test]
    fn a_machine_named_fleet_fails() {
        let mut document = document(json!([]), json!([]));
        document["host"] = json!("fleet");
        assert!(problems(document)[0].starts_with("host: `fleet` names the fleet's own mark"));
    }

    #[test]
    fn a_proxied_record_with_a_ttl_fails() {
        let mut proxied = record("example.com", "A", "192.0.2.1");
        proxied["proxied"] = json!(true);
        proxied["ttl"] = json!(300);
        assert!(problems(document(json!([proxied]), json!([])))[0].contains("always auto"));
    }
}
