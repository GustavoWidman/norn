use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::json;
use tempfile::TempDir;

use crate::{
    desired::{Desired, Mode},
    dns::Record,
    kind::{Ladder, Want},
    model::{HostName, Zones},
    run::{Cloudflare, Lock, Timing, converge_with, report_with},
    status::{self, Health},
};

use super::{
    fake,
    fixture::{WEB_1, WEB_2, baseline_records, host, proxied, want, web_1, web_2, wire},
};

struct Machine {
    dir: TempDir,
    host: &'static str,
}

impl Machine {
    fn new(host: &'static str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), "test-token\n").unwrap();
        Self { dir, host }
    }

    fn declare(&self, mode: Mode, wants: Vec<Want<crate::dns::Dns>>) -> PathBuf {
        let records: Vec<Record> = wants.into_iter().map(|want| want.value).collect();
        let zones = ["example.com", "example.net", "example.org"].map(String::from);
        let desired = Desired {
            host: HostName::parse(self.host).unwrap(),
            mode,
            zones: Zones::parse(&zones).unwrap(),
            dns: records,
        };
        let raw = self.dir.path().join("raw.json");
        std::fs::write(&raw, desired.canonical()).unwrap();
        let path = self.dir.path().join("desired.json");
        crate::run::check(&raw, Some(&path)).unwrap();
        path
    }

    fn token(&self) -> PathBuf {
        self.dir.path().join("token")
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn converge(
        &self,
        fake: &Arc<fake::Cloudflare>,
        mode: Mode,
        wants: Vec<Want<crate::dns::Dns>>,
    ) -> Health {
        let desired = self.declare(mode, wants);
        converge_with(
            &Cloudflare::Fake(fake.clone()),
            &desired,
            Some(&self.token()),
            &self.state(),
            timing(),
        )
    }

    fn report(&self, fake: &Arc<fake::Cloudflare>, wants: Vec<Want<crate::dns::Dns>>) -> String {
        let desired = self.declare(Mode::Report, wants);
        let (text, complete) = report_with(
            &Cloudflare::Fake(fake.clone()),
            &desired,
            Some(&self.token()),
            timing(),
            false,
        )
        .unwrap();
        assert!(complete);
        text
    }
}

fn timing() -> Timing {
    Timing {
        deadline: Duration::from_secs(30),
        per_request: Duration::from_secs(5),
        lock_wait: Duration::from_secs(1),
    }
}

fn cloudflare() -> Arc<fake::Cloudflare> {
    let fake = fake::Cloudflare::new(3);
    for (zone, records) in baseline_records() {
        for record in records {
            fake.seed(zone, record.json());
        }
    }
    fake
}

const ADOPT: Mode = Mode::Write(Ladder::Adopt);
const ENFORCE: Mode = Mode::Write(Ladder::Enforce);

#[test]
fn report_reads_every_page_and_writes_nothing() {
    let fake = cloudflare();
    let one = Machine::new(WEB_1).report(&fake, web_1());
    let two = Machine::new(WEB_2).report(&fake, web_2());
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    assert!(one.contains("32 objects read"), "{one}");
    assert!(one.contains("\n5 to adopt · 0 to update · 0 to delete · 0 to create · 0 withheld · 0 blocked · 27 untouched"), "{one}");
    assert!(two.contains("\n18 to adopt · 0 to update · 0 to delete · 0 to create · 0 withheld · 0 blocked · 14 untouched"), "{two}");
    assert!(
        one.contains("untouched         A      status.example.com"),
        "{one}"
    );
}

#[test]
fn adopt_only_marks_and_the_next_runs_write_nothing() {
    let fake = cloudflare();
    let (one, two) = (Machine::new(WEB_1), Machine::new(WEB_2));
    assert_eq!(one.converge(&fake, ADOPT, web_1()), Health::Converged);
    assert_eq!(two.converge(&fake, ADOPT, web_2()), Health::Converged);
    let writes = fake.writes();
    assert_eq!(writes.len(), 23);
    let marks = |host: &str| {
        let mark = format!(r#"{{"comment":"norn v1 host={host}"}}"#);
        writes
            .iter()
            .filter(|write| write.starts_with("PATCH ") && write.ends_with(&mark))
            .count()
    };
    assert_eq!((marks(WEB_1), marks(WEB_2)), (5, 18), "{writes:#?}");

    for (machine, wants) in [(&one, web_1()), (&two, web_2())] {
        assert_eq!(machine.converge(&fake, ENFORCE, wants), Health::Converged);
    }
    assert_eq!(fake.writes().len(), 23);
    let status = status::read(&two.state()).unwrap();
    assert_eq!(
        (status.health, status.counts.kept, status.troubled_since),
        (Health::Converged, 18, None)
    );
}

#[test]
fn enforce_rewrites_creates_and_deletes_its_own_then_settles() {
    let fake = cloudflare();
    let one = Machine::new(WEB_1);
    one.converge(&fake, ADOPT, web_1());
    let me = host(WEB_1);
    let changed = vec![
        want(&me, "A", "cache.example.com", "192.0.2.11"),
        proxied(&me, "A", "heimdall.example.com", "192.0.2.10"),
        want(&me, "A", "example.org", "192.0.2.30"),
        proxied(&me, "CNAME", "www.example.org", "example.org"),
        want(&me, "TXT", "_verify.example.org", "verify=1"),
    ];
    let before = fake.writes().len();
    assert_eq!(
        one.converge(&fake, ENFORCE, changed.clone()),
        Health::Converged
    );
    let writes: Vec<String> = fake.writes().split_off(before);
    let verbs: Vec<&str> = writes
        .iter()
        .map(|write| write.split(' ').next().unwrap())
        .collect();
    assert_eq!(verbs, ["PUT", "DELETE", "POST"], "{writes:#?}");

    let before = fake.writes().len();
    assert_eq!(one.converge(&fake, ENFORCE, changed), Health::Converged);
    assert_eq!(fake.writes().len(), before);
    let txt = fake
        .records("example.org")
        .into_iter()
        .find(|record| record["name"] == "_verify.example.org")
        .unwrap();
    assert_eq!(txt["comment"], "norn v1 host=web-1");
}

#[test]
fn an_update_keeps_what_norn_does_not_model() {
    let fake = cloudflare();
    let me = host(WEB_1);
    let mut record = wire("CNAME", "app.example.com", "example.com")
        .marked(&me)
        .json();
    record["settings"] = json!({ "flatten_cname": true });
    record["tags"] = json!(["team:web"]);
    fake.seed("example.com", record);

    let wants = vec![want(&me, "CNAME", "app.example.com", "www.example.com")];
    assert_eq!(
        Machine::new(WEB_1).converge(&fake, ENFORCE, wants),
        Health::Converged
    );
    let app = fake
        .records("example.com")
        .into_iter()
        .find(|record| record["name"] == "app.example.com")
        .unwrap();
    assert_eq!(app["content"], "www.example.com");
    assert_eq!(app["settings"], json!({ "flatten_cname": true }));
    assert_eq!(app["tags"], json!(["team:web"]));
}

#[test]
fn adopt_and_report_hold_every_other_step() {
    let fake = cloudflare();
    let one = Machine::new(WEB_1);
    let mut wants = web_1();
    wants.push(want(&host(WEB_1), "A", "new.example.com", "192.0.2.10"));
    assert_eq!(
        one.converge(&fake, Mode::Report, wants.clone()),
        Health::Held
    );
    assert!(fake.writes().is_empty());
    assert_eq!(one.converge(&fake, ADOPT, wants), Health::Held);
    assert_eq!(fake.writes().len(), 5);
    let status = status::read(&one.state()).unwrap();
    assert_eq!(
        status.lines,
        ["held: create dns record example.com: web-1 A      new.example.com 192.0.2.10  dns only"]
    );
}

#[test]
fn a_run_past_its_deadline_sends_nothing_and_records_the_streak() {
    let fake = cloudflare();
    let one = Machine::new(WEB_1);
    let desired = one.declare(ENFORCE, web_1());
    let spent = Timing {
        deadline: Duration::ZERO,
        ..timing()
    };
    let run = || {
        converge_with(
            &Cloudflare::Fake(fake.clone()),
            &desired,
            Some(&one.token()),
            &one.state(),
            spent,
        )
    };
    assert_eq!(run(), Health::Failing);
    assert_eq!(fake.requests(), 0);
    let first = status::read(&one.state()).unwrap();
    assert_eq!(first.counts.unread, 3);
    assert_eq!(run(), Health::Failing);
    let second = status::read(&one.state()).unwrap();
    assert_eq!(second.troubled_since, first.troubled_since);
    assert!(second.troubled_since.is_some());
    assert!(!second.fresh(
        second.finished + chrono::TimeDelta::minutes(31),
        Duration::from_secs(30 * 60)
    ));
    assert!(second.fresh(
        second.finished + chrono::TimeDelta::minutes(29),
        Duration::from_secs(30 * 60)
    ));
}

#[test]
fn a_missing_token_skips_the_run() {
    let fake = cloudflare();
    let one = Machine::new(WEB_1);
    let desired = one.declare(ENFORCE, web_1());
    let missing = one.dir.path().join("not-decrypted-yet");
    let health = converge_with(
        &Cloudflare::Fake(fake.clone()),
        &desired,
        Some(&missing),
        &one.state(),
        timing(),
    );
    assert_eq!(health, Health::Skipped);
    assert_eq!(fake.requests(), 0);
    let status = status::read(&one.state()).unwrap();
    assert_eq!(
        (status.health, status.mode.as_deref()),
        (Health::Skipped, Some("enforce"))
    );
}

#[test]
fn a_run_that_cannot_take_the_lock_leaves_the_work_to_the_next() {
    let fake = cloudflare();
    let one = Machine::new(WEB_1);
    let desired = one.declare(ENFORCE, web_1());
    let _held = Lock::acquire(&one.state(), Duration::ZERO)
        .unwrap()
        .unwrap();
    let waiting = Timing {
        lock_wait: Duration::from_millis(200),
        ..timing()
    };
    let health = converge_with(
        &Cloudflare::Fake(fake.clone()),
        &desired,
        Some(&one.token()),
        &one.state(),
        waiting,
    );
    assert_eq!(health, Health::Skipped);
    assert_eq!(fake.requests(), 0);
}

#[test]
fn an_unreadable_desired_file_fails_the_run_not_the_process() {
    let one = Machine::new(WEB_1);
    let desired: &Path = &one.dir.path().join("missing.json");
    let health = converge_with(
        &Cloudflare::Fake(cloudflare()),
        desired,
        Some(&one.token()),
        &one.state(),
        timing(),
    );
    assert_eq!(health, Health::Failing);
}
