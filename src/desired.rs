use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    dns::{Dns, Record, RecordType, Slot},
    kind::{Ladder, Want},
    model::{Fqdn, HostName, Owner, Revision, Zones},
    workers::{
        Account, CompatibilityDate, Module, Pattern, Route, Routes, Script, ScriptName, Scripts,
        fits,
    },
};

const SCHEMA: u32 = 4;

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
    /// the account the fleet's workers live in. with none, norn reads no
    /// workers at all
    pub account: Option<Account>,
    pub dns: Vec<Record>,
    pub fleet: Fleet,
}

/// what every machine of the fleet declares alike
#[derive(Clone, PartialEq, Debug, Default)]
pub(crate) struct Fleet {
    /// none for a configuration built from an uncommitted tree
    pub revision: Option<Revision>,
    pub dns: Vec<Record>,
    pub workers: Vec<Worker>,
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Worker {
    pub name: ScriptName,
    pub module: PathBuf,
    /// the module's bytes, read along with the desired file
    pub source: Arc<[u8]>,
    pub compatibility_date: CompatibilityDate,
    pub routes: Vec<Pattern>,
    /// patterns that run no worker, so requests there skip this one
    pub bypass: Vec<Pattern>,
}

impl Worker {
    fn script(&self) -> Script {
        Script {
            name: self.name.clone(),
            compatibility_date: self.compatibility_date,
            module: Module::declared(self.source.clone()),
            routes: self.routes.iter().cloned().collect(),
            bypass: self.bypass.iter().cloned().collect(),
        }
    }
}

pub(crate) struct Wants {
    pub dns: Vec<Want<Dns>>,
    pub scripts: Vec<Want<Scripts>>,
    pub routes: Vec<Want<Routes>>,
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
        let garbled = |error: serde_json::Error| {
            Problems(vec![Problem {
                subject: "the desired file".into(),
                message: error.to_string(),
            }])
        };
        let document::Schema { schema } = serde_json::from_slice(bytes).map_err(garbled)?;
        if schema != SCHEMA {
            return Err(Problems(vec![Problem {
                subject: "schema".into(),
                message: format!(
                    "this norn reads schema {SCHEMA}, not {schema}; rebuild the desired file with this norn's module"
                ),
            }]));
        }
        let document: document::Document = serde_json::from_slice(bytes).map_err(garbled)?;

        let mut problems = Vec::new();
        let mut problem = |subject: &str, message: String| {
            problems.push(Problem {
                subject: subject.to_owned(),
                message,
            });
        };

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

        let mut records = |rows: &[document::Record], scope: &str| {
            let mut records = Vec::new();
            for declared in rows {
                let subject = format!("{scope} {} {}", declared.name, declared.kind);
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
                    (Ok(record), _) => records.push(record),
                }
            }
            records.sort_by_key(|record| (record.slot(), record.data.content()));
            for (subject, message) in collisions(scope, &records) {
                problem(&subject, message);
            }
            records
        };
        let dns = records(&document.dns, "dns");
        let fleet_dns = records(&document.fleet.dns, "fleet dns");
        for (subject, message) in across(&dns, &fleet_dns) {
            problem(&subject, message);
        }

        let account = document
            .account
            .as_deref()
            .map(|account| Account::parse(account).map_err(|message| problem("account", message)));
        let mut workers = Vec::new();
        let mut patterns = BTreeSet::new();
        for declared in &document.fleet.workers {
            let subject = format!("fleet worker {}", declared.name);
            let name =
                ScriptName::parse(&declared.name).map_err(|message| problem(&subject, message));
            let source = std::fs::read(&declared.module)
                .map(Arc::from)
                .map_err(|error| problem(&subject, format!("{}: {error}", declared.module)));
            let date = CompatibilityDate::parse(&declared.compatibility_date)
                .map_err(|message| problem(&subject, message));
            let mut parsed = |listed: &[String]| -> Vec<Pattern> {
                let mut found = Vec::new();
                for pattern in listed {
                    match zones
                        .as_ref()
                        .map(|zones| Pattern::declared(pattern, zones))
                    {
                        Some(Ok(pattern)) if !patterns.insert(pattern.clone()) => {
                            problem(
                                &subject,
                                format!("{pattern} is declared twice; a pattern has one route"),
                            );
                        }
                        Some(Ok(pattern)) => found.push(pattern),
                        Some(Err(message)) => problem(&subject, message),
                        None => {}
                    }
                }
                found.sort();
                found
            };
            let routes = parsed(&declared.routes);
            let bypass = parsed(&declared.bypass);
            if let (Ok(name), Ok(source), Ok(compatibility_date)) = (name, source, date) {
                let worker = Worker {
                    name,
                    module: PathBuf::from(&declared.module),
                    source,
                    compatibility_date,
                    routes,
                    bypass,
                };
                match fits(&worker.script()) {
                    Ok(()) => workers.push(worker),
                    Err(message) => problem(&subject, message),
                }
            }
        }
        workers.sort_by(|a, b| a.name.cmp(&b.name));
        if workers.windows(2).any(|pair| pair[0].name == pair[1].name) {
            problem("fleet workers", "a worker is declared twice".into());
        }
        if account.is_none() && !document.fleet.workers.is_empty() {
            problem(
                "account",
                "fleet workers need cloudflare.account, the account their scripts live in".into(),
            );
        }
        let fleet = Fleet {
            revision: document.fleet.revision.map(Revision),
            dns: fleet_dns,
            workers,
        };

        match (host, mode, zones, account.transpose()) {
            (Some(host), Some(mode), Some(zones), Ok(account)) if problems.is_empty() => Ok(Self {
                host,
                mode,
                zones,
                account,
                dns,
                fleet,
            }),
            _ => Err(Problems(problems)),
        }
    }

    pub fn canonical(&self) -> String {
        let document = document::Document {
            schema: SCHEMA,
            host: self.host.to_string(),
            mode: self.mode.as_str().to_owned(),
            zones: self.zones.iter().map(ToString::to_string).collect(),
            account: self.account.as_ref().map(ToString::to_string),
            dns: self.dns.iter().map(document::Record::from).collect(),
            fleet: document::Fleet {
                revision: self.fleet.revision.map(|Revision(revision)| revision),
                dns: self.fleet.dns.iter().map(document::Record::from).collect(),
                workers: self
                    .fleet
                    .workers
                    .iter()
                    .map(document::Worker::from)
                    .collect(),
            },
        };
        let mut text = serde_json::to_string_pretty(&document).expect("the document serializes");
        text.push('\n');
        text
    }

    pub fn wants(&self) -> Wants {
        let host = Owner::Host(self.host.clone());
        let dns = (self.dns.iter().map(|record| (&host, record)))
            .chain(self.fleet.dns.iter().map(|record| (&Owner::Fleet, record)))
            .map(|(owner, record)| Want {
                scope: self
                    .zones
                    .zone_of(&record.name)
                    .expect("parse checked every name's zone")
                    .clone(),
                owner: owner.clone(),
                value: record.clone(),
            })
            .collect();
        let shared = |value| Want {
            scope: self
                .account
                .clone()
                .expect("parse checked the workers' account"),
            owner: Owner::Fleet,
            value,
        };
        let scripts = self
            .fleet
            .workers
            .iter()
            .map(|worker| shared(worker.script()))
            .collect();
        let routes = self
            .fleet
            .workers
            .iter()
            .flat_map(|worker| {
                let runs = worker
                    .routes
                    .iter()
                    .map(|pattern| (pattern, Some(&worker.name)));
                let skips = worker.bypass.iter().map(|pattern| (pattern, None));
                runs.chain(skips)
            })
            .map(|(pattern, script)| Want {
                scope: pattern
                    .zone(&self.zones)
                    .expect("parse checked every pattern's zone")
                    .clone(),
                owner: Owner::Fleet,
                value: Route {
                    pattern: pattern.clone(),
                    script: script.cloned(),
                },
            })
            .collect();
        Wants {
            dns,
            scripts,
            routes,
        }
    }
}

fn collisions(scope: &str, records: &[Record]) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut slots: BTreeMap<Slot, Vec<&Record>> = BTreeMap::new();
    for record in records {
        slots.entry(record.slot()).or_default().push(record);
    }
    for (slot, records) in &slots {
        let subject = format!("{scope} {} {}", slot.name, slot.kind.as_str());
        let mut contents: Vec<_> = records.iter().map(|record| record.data.content()).collect();
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
                format!("{scope} {name} CNAME"),
                format!(
                    "a CNAME shares its name with nothing, and {name} also declares {}",
                    others.join(", ")
                ),
            ));
        }
    }
    found
}

/// a name and type has one owner, and a CNAME shares its name with nothing,
/// whether this machine or the fleet declares what is beside it
fn across(host: &[Record], fleet: &[Record]) -> Vec<(String, String)> {
    let slots = |records: &[Record]| records.iter().map(Record::slot).collect::<BTreeSet<_>>();
    let (host, fleet) = (slots(host), slots(fleet));
    let subject = |slot: &Slot| format!("fleet dns {} {}", slot.name, slot.kind.as_str());
    let mut found: Vec<_> = host
        .intersection(&fleet)
        .map(|slot| {
            (
                subject(slot),
                "this machine declares it too; one name and type has one owner".into(),
            )
        })
        .collect();
    for slot in &fleet {
        let beside: Vec<&str> = host
            .iter()
            .filter(|other| other.name == slot.name && other.kind != slot.kind)
            .filter(|other| slot.kind == RecordType::Cname || other.kind == RecordType::Cname)
            .map(|other| other.kind.as_str())
            .collect();
        if !beside.is_empty() {
            found.push((
                subject(slot),
                format!(
                    "a CNAME shares its name with nothing, and this machine declares {} at {}",
                    beside.join(", "),
                    slot.name
                ),
            ));
        }
    }
    found
}

mod document {
    use serde::{Deserialize, Serialize};

    use crate::dns::{Rdata, Record as Value, Ttl};

    use super::Worker as Declared;

    #[derive(Deserialize)]
    pub struct Schema {
        pub schema: u32,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct Document {
        pub schema: u32,
        pub host: String,
        pub mode: String,
        pub zones: Vec<String>,
        pub account: Option<String>,
        pub dns: Vec<Record>,
        pub fleet: Fleet,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct Fleet {
        pub revision: Option<u64>,
        pub dns: Vec<Record>,
        pub workers: Vec<Worker>,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct Worker {
        pub name: String,
        pub module: String,
        pub compatibility_date: String,
        pub routes: Vec<String>,
        pub bypass: Vec<String>,
    }

    impl From<&Declared> for Worker {
        fn from(worker: &Declared) -> Self {
            let shown = |patterns: &[crate::workers::Pattern]| {
                patterns.iter().map(ToString::to_string).collect()
            };
            Worker {
                name: worker.name.to_string(),
                module: worker.module.display().to_string(),
                compatibility_date: worker.compatibility_date.to_string(),
                routes: shown(&worker.routes),
                bypass: shown(&worker.bypass),
            }
        }
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

    fn document(dns: serde_json::Value) -> serde_json::Value {
        shared(dns, json!(null), json!([]))
    }

    fn shared(
        dns: serde_json::Value,
        revision: serde_json::Value,
        fleet: serde_json::Value,
    ) -> serde_json::Value {
        json!({
            "schema": 4,
            "host": "web-1",
            "mode": "report",
            "zones": ["example.com", "example.org"],
            "account": null,
            "dns": dns,
            "fleet": { "revision": revision, "dns": fleet, "workers": [] },
        })
    }

    const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";

    fn workers(dir: &Path, workers: serde_json::Value) -> serde_json::Value {
        let module = dir.join("worker.js");
        std::fs::write(&module, "export default { fetch() {} }").unwrap();
        let mut document = document(json!([]));
        document["account"] = json!(ACCOUNT);
        document["fleet"]["revision"] = json!(1794942959);
        document["fleet"]["workers"] = workers;
        let module = module.display().to_string();
        let text = document.to_string().replace("$MODULE", &module);
        serde_json::from_str(&text).unwrap()
    }

    fn worker(name: &str, routes: &[&str], bypass: &[&str]) -> serde_json::Value {
        json!({
            "name": name,
            "module": "$MODULE",
            "compatibility_date": "2026-11-01",
            "routes": routes,
            "bypass": bypass,
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
        let document = shared(
            json!([
                record("www.example.com", "CNAME", "example.com"),
                long,
                proxied,
                record("example.com", "MX", "mail.example.com"),
                record("example.com", "TXT", "\"quoted\" text"),
                record("status.example.org", "A", "192.0.2.9"),
            ]),
            json!(1794942959),
            json!([
                record("coeus.example.com", "CNAME", "tunnel.example.net"),
                record("example.org", "A", "192.0.2.9"),
            ]),
        );
        let desired = Desired::parse(&serde_json::to_vec(&document).unwrap()).unwrap();
        assert_eq!(desired.fleet.revision, Some(Revision(1794942959)));
        assert_eq!(desired.fleet.dns.len(), 2);
        let canonical = desired.canonical();
        let again = Desired::parse(canonical.as_bytes()).unwrap();
        assert_eq!(again, desired);
        assert_eq!(again.canonical(), canonical);
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let found = problems(document(json!([
            record("blog.example.net", "A", "192.0.2.1"),
            record("Blog.example.com", "A", "192.0.2.1"),
            record("api.example.com", "A", "192.0.2.300"),
            record("x.example.com", "SRV", "0 5 5060 sip.example.com"),
        ])));
        assert_eq!(found.len(), 4, "{found:#?}");
        assert!(found[0].contains("outside every managed zone (example.com, example.org)"));
        assert!(found[1].contains("canonical"));
        assert!(found[2].contains("not an IPv4 address"));
        assert!(found[3].contains("not SRV"));
    }

    #[test]
    fn a_record_is_declared_once_and_a_cname_stands_alone() {
        let found = problems(document(json!([
            record("app.example.com", "A", "192.0.2.1"),
            record("app.example.com", "A", "192.0.2.1"),
            record("www.example.com", "CNAME", "example.com"),
            record("www.example.com", "TXT", "hello"),
        ])));
        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(found[0].contains("declared twice"));
        assert!(found[1].contains("CNAME shares its name with nothing"));
    }

    #[test]
    fn a_name_and_type_has_one_owner_across_the_machine_and_the_fleet() {
        let found = problems(shared(
            json!([
                record("app.example.com", "A", "192.0.2.1"),
                record("www.example.com", "TXT", "hello"),
                record("api.example.com", "CNAME", "example.com"),
            ]),
            json!(1),
            json!([
                record("app.example.com", "A", "192.0.2.2"),
                record("www.example.com", "CNAME", "example.com"),
                record("api.example.com", "AAAA", "2001:db8::1"),
                record("tunnel.example.com", "CNAME", "a.example.org"),
                record("tunnel.example.com", "CNAME", "b.example.org"),
            ]),
        ));
        assert_eq!(
            found,
            [
                "fleet dns tunnel.example.com CNAME: a name holds one CNAME",
                "fleet dns app.example.com A: this machine declares it too; one name and type has one owner",
                "fleet dns api.example.com AAAA: a CNAME shares its name with nothing, and this machine declares CNAME at api.example.com",
                "fleet dns www.example.com CNAME: a CNAME shares its name with nothing, and this machine declares TXT at www.example.com",
            ]
        );
    }

    #[test]
    fn a_file_of_an_older_schema_is_refused() {
        let found = problems(json!({
            "schema": 3,
            "host": "web-1",
            "mode": "report",
            "zones": ["example.com"],
            "dns": [],
            "fleet": { "revision": null, "dns": [] },
        }));
        assert_eq!(
            found,
            [
                "schema: this norn reads schema 4, not 3; rebuild the desired file with this norn's module"
            ]
        );
    }

    #[test]
    fn a_worker_reads_its_module_and_its_canonical_form_parses_back() {
        let dir = tempfile::tempdir().unwrap();
        let document = workers(
            dir.path(),
            json!([worker(
                "coeus-offline",
                &["coeus.example.com/*"],
                &["coeus.example.com/live/*", "coeus.example.com/api/*"]
            )]),
        );
        let desired = Desired::parse(&serde_json::to_vec(&document).unwrap()).unwrap();
        let worker = &desired.fleet.workers[0];
        assert_eq!(&*worker.source, b"export default { fetch() {} }");
        assert_eq!(
            worker
                .bypass
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["coeus.example.com/api/*", "coeus.example.com/live/*"]
        );
        let again = Desired::parse(desired.canonical().as_bytes()).unwrap();
        assert_eq!(again, desired);

        let wants = desired.wants();
        assert_eq!(wants.scripts.len(), 1);
        let routes: Vec<String> = wants
            .routes
            .iter()
            .map(|want| format!("{} {} {}", want.scope, want.value.pattern, want.value))
            .collect();
        assert_eq!(
            routes,
            [
                "example.com coeus.example.com/* runs coeus-offline",
                "example.com coeus.example.com/api/* runs no worker",
                "example.com coeus.example.com/live/* runs no worker",
            ]
        );
    }

    #[test]
    fn a_worker_whose_mark_would_not_fit_in_its_tags_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let routes: Vec<String> = (0..60)
            .map(|n| format!("site-{n:02}.example.com/*"))
            .collect();
        let routes: Vec<&str> = routes.iter().map(String::as_str).collect();
        let found = problems(workers(
            dir.path(),
            json!([worker("coeus-offline", &routes, &[])]),
        ));
        assert_eq!(
            found,
            [
                "fleet worker coeus-offline: its routes and bypasses take 11 tags, and cloudflare keeps 10 on a script; declare fewer"
            ]
        );
    }

    #[test]
    fn a_worker_with_a_missing_module_a_stray_route_or_no_account_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut document = workers(
            dir.path(),
            json!([
                worker(
                    "coeus-offline",
                    &["coeus.example.com/*"],
                    &["coeus.example.net/api/*"]
                ),
                worker("Other", &["coeus.example.com/*"], &[]),
            ]),
        );
        document["fleet"]["workers"][1]["module"] = json!("/nonexistent/worker.js");
        document["fleet"]["workers"][1]["compatibility_date"] = json!("2026-13-01");
        document["account"] = json!(null);
        let found = problems(document);
        assert_eq!(
            found,
            [
                "fleet worker coeus-offline: coeus.example.net/api/* is outside every managed zone (example.com, example.org)",
                "fleet worker Other: \"Other\" has a character outside [a-z0-9_-]",
                "fleet worker Other: /nonexistent/worker.js: No such file or directory (os error 2)",
                "fleet worker Other: \"2026-13-01\" is not a date written YYYY-MM-DD",
                "fleet worker Other: coeus.example.com/* is declared twice; a pattern has one route",
                "account: fleet workers need cloudflare.account, the account their scripts live in",
            ]
        );
    }

    #[test]
    fn an_unknown_field_fails() {
        let mut document = document(json!([]));
        document["note"] = json!("x");
        assert!(problems(document)[0].contains("unknown field"));
        let mut extra = record("example.com", "A", "192.0.2.1");
        extra["comment"] = json!("x");
        assert!(problems(super::tests::document(json!([extra])))[0].contains("unknown field"));
    }

    #[test]
    fn a_proxied_record_with_a_ttl_fails() {
        let mut proxied = record("example.com", "A", "192.0.2.1");
        proxied["proxied"] = json!(true);
        proxied["ttl"] = json!(300);
        assert!(problems(document(json!([proxied])))[0].contains("always auto"));
    }
}
