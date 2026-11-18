//! shared records, written by machines that run different revisions of the
//! configuration at once

use std::{sync::Arc, time::Duration};

use serde_json::Value;

use crate::{
    dns::Dns,
    kind::Want,
    status::{self, Health},
};

use super::{
    fake,
    fixture::{WEB_1, WEB_2, host, shared, want, wire},
    run::{ENFORCE, Machine, cloudflare},
};

const OLD: u64 = 1_794_900_000;
const NEW: u64 = 1_794_942_959;
const NEWER: u64 = 1_795_000_000;

const FRESH: Duration = Duration::from_secs(60);

fn tunnel(target: &str) -> Vec<Want<Dns>> {
    vec![
        shared("CNAME", "coeus.example.com", target),
        shared("TXT", "fleet.example.org", "fleet=1"),
    ]
}

fn at(revision: u64, host: &'static str) -> Machine {
    let mut machine = Machine::new(host);
    machine.revision = Some(revision);
    machine
}

fn named(fake: &fake::Cloudflare, zone: &str, name: &str) -> Vec<Value> {
    fake.records(zone)
        .into_iter()
        .filter(|record| record["name"] == name)
        .collect()
}

fn coeus(fake: &fake::Cloudflare) -> (String, String) {
    match named(fake, "example.com", "coeus.example.com").as_slice() {
        [record] => (
            record["content"].as_str().unwrap().to_owned(),
            record["comment"].as_str().unwrap().to_owned(),
        ),
        other => panic!("{other:#?}"),
    }
}

fn lines(machine: &Machine) -> Vec<String> {
    status::read(&machine.state()).unwrap().lines
}

#[test]
fn two_revisions_settle_on_the_newer_and_never_flip_back() {
    let fake = cloudflare();
    let (old, new) = (at(OLD, WEB_1), at(NEW, WEB_2));

    assert_eq!(
        old.converge(&fake, ENFORCE, tunnel("old.example.net")),
        Health::Converged
    );
    assert_eq!(
        coeus(&fake),
        ("old.example.net".into(), format!("norn v1 fleet t={OLD}"))
    );
    assert_eq!(
        new.converge(&fake, ENFORCE, tunnel("new.example.net")),
        Health::Converged
    );
    let settled = (
        "new.example.net".to_owned(),
        format!("norn v1 fleet t={NEW}"),
    );
    assert_eq!(coeus(&fake), settled);
    let txt = named(&fake, "example.org", "fleet.example.org")
        .into_iter()
        .find(|record| record["type"] == "TXT" && record["content"] == "fleet=1")
        .unwrap();
    assert_eq!(
        txt["comment"],
        format!("norn v1 fleet t={OLD}"),
        "a record the newer configuration keeps as it is is not rewritten"
    );

    let writes = fake.writes().len();
    for _ in 0..3 {
        assert_eq!(
            old.converge(&fake, ENFORCE, tunnel("old.example.net")),
            Health::Held
        );
        assert_eq!(
            new.converge(&fake, ENFORCE, tunnel("new.example.net")),
            Health::Converged
        );
    }
    assert_eq!(fake.writes().len(), writes, "{:#?}", fake.writes());
    assert_eq!(coeus(&fake), settled);
    assert_eq!(
        lines(&old),
        [format!(
            "held: update dns record example.com: fleet CNAME  coeus.example.com new.example.net  dns only -> old.example.net  dns only: a shared object carries t={NEW}, newer than this configuration, whose machines write the shared objects"
        )]
    );
}

#[test]
fn an_older_revision_leaves_a_newer_ones_records_alone() {
    let fake = cloudflare();
    let mut new = at(NEW, WEB_2);
    new.rolled_out(OLD, FRESH);
    let declared = vec![
        shared("CNAME", "coeus.example.com", "new.example.net"),
        shared("A", "vpn.example.com", "192.0.2.50"),
    ];
    new.converge(&fake, ENFORCE, declared);
    let before = fake.records("example.com");

    let mut old = at(OLD, WEB_1);
    old.rolled_out(OLD, FRESH);
    let health = old.converge(
        &fake,
        ENFORCE,
        vec![
            shared("CNAME", "coeus.example.com", "old.example.net"),
            shared("A", "legacy.example.com", "192.0.2.60"),
        ],
    );
    assert_eq!(health, Health::Held);
    assert_eq!(fake.records("example.com"), before);
    let held = lines(&old);
    assert_eq!(held.len(), 3, "an update, a retire and a create: {held:#?}");
    assert!(
        held.iter().all(|line| line.starts_with("held: ")),
        "{held:#?}"
    );
}

#[test]
fn a_dropped_record_retires_then_goes_once_the_whole_fleet_runs_its_successor() {
    let fake = cloudflare();
    let before = vec![
        shared("CNAME", "coeus.example.com", "tunnel.example.net"),
        shared("A", "vpn.example.com", "192.0.2.50"),
    ];
    let after = || vec![shared("CNAME", "coeus.example.com", "tunnel.example.net")];
    let vpn = || named(&fake, "example.com", "vpn.example.com");

    let mut first = at(OLD, WEB_1);
    first.rolled_out(OLD, FRESH);
    first.converge(&fake, ENFORCE, before.clone());

    first.revision = Some(NEW);
    assert_eq!(first.converge(&fake, ENFORCE, after()), Health::Converged);
    assert_eq!(vpn()[0]["comment"], format!("norn v1 fleet t={NEW} delete"));

    assert_eq!(first.converge(&fake, ENFORCE, after()), Health::Held);
    assert_eq!(
        vpn().len(),
        1,
        "it waits while the fleet still runs t={OLD}"
    );
    assert!(
        lines(&first)[0].ends_with(&format!(
            "it is deleted once the whole fleet runs t={NEW}, the configuration that dropped it; the fleet runs t={OLD}"
        )),
        "{:#?}",
        lines(&first)
    );

    let mut second = at(OLD, WEB_2);
    second.rolled_out(OLD, FRESH);
    assert_eq!(
        second.converge(&fake, ENFORCE, before.clone()),
        Health::Held
    );
    assert_eq!(vpn()[0]["comment"], format!("norn v1 fleet t={NEW} delete"));

    first.rolled_out(NEW, FRESH);
    assert_eq!(first.converge(&fake, ENFORCE, after()), Health::Converged);
    assert!(vpn().is_empty());

    second.rolled_out(NEW, FRESH);
    let writes = fake.writes().len();
    assert_eq!(second.converge(&fake, ENFORCE, before), Health::Held);
    assert_eq!(fake.writes().len(), writes, "{:#?}", fake.writes());
    assert!(
        vpn().is_empty(),
        "a machine behind the rollout recreated it"
    );
    assert!(
        lines(&second)[0].ends_with(&format!(
            "the fleet has rolled out t={NEW}, newer than this configuration"
        )),
        "{:#?}",
        lines(&second)
    );
}

#[test]
fn without_a_rollout_file_a_retiring_record_is_never_deleted() {
    let fake = cloudflare();
    fake.seed(
        "example.com",
        wire("A", "vpn.example.com", "192.0.2.50")
            .retiring(OLD)
            .json(),
    );
    let machine = at(NEW, WEB_1);
    for _ in 0..2 {
        assert_eq!(machine.converge(&fake, ENFORCE, vec![]), Health::Held);
    }
    assert!(fake.writes().is_empty(), "{:#?}", fake.writes());
    assert_eq!(
        lines(&machine),
        [format!(
            "held: delete dns record example.com: fleet A      vpn.example.com 192.0.2.50  dns only: no rollout file is configured, so norn never deletes it; delete it by hand once every machine runs t={OLD}, the configuration that dropped it"
        )]
    );
}

#[test]
fn a_held_fleet_says_why_a_retiring_record_waits_and_never_to_delete_it_by_hand() {
    let uncommitted = Machine::new(WEB_1);
    let mut unknown = at(NEW, WEB_1);
    unknown.rolled_out(NEW, FRESH);
    std::fs::remove_file(&unknown.rollout.as_ref().unwrap().path).unwrap();
    let mut behind = at(NEW, WEB_1);
    behind.rolled_out(NEWER, FRESH);
    let older = at(OLD, WEB_1);
    let cases = [
        (uncommitted, "built from an uncommitted tree".to_owned()),
        (unknown, "the fleet's rollout is unknown: ".to_owned()),
        (behind, format!("the fleet has rolled out t={NEWER}")),
        (older, format!("a shared object carries t={NEW}")),
    ];
    let retiring = wire("A", "vpn.example.com", "192.0.2.50").retiring(NEW);
    for (machine, why) in cases {
        let fake = cloudflare();
        fake.seed("example.com", retiring.json());
        assert_eq!(machine.converge(&fake, ENFORCE, vec![]), Health::Held);
        let held = lines(&machine);
        assert!(
            matches!(held.as_slice(), [line] if line.starts_with("held: delete ")
                && line.contains(&why)
                && !line.contains("by hand")),
            "{why}: {held:#?}"
        );
    }

    let fake = fake::Cloudflare::new(100);
    fake.seed("example.com", retiring.json());
    fake.seed("example.org", wire("TXT", "example.org", "hello").json());
    let machine = at(NEW, WEB_1);
    assert_eq!(machine.converge(&fake, ENFORCE, vec![]), Health::Failing);
    let held: Vec<String> = lines(&machine)
        .into_iter()
        .filter(|line| line.starts_with("held: "))
        .collect();
    assert!(
        matches!(held.as_slice(), [line] if line.starts_with("held: delete ")
            && line.ends_with("a scope could not be read, so a newer configuration's shared objects may be there unseen")),
        "{held:#?}"
    );
}

#[test]
fn a_retiring_record_declared_again_gets_its_plain_mark_back() {
    let fake = cloudflare();
    fake.seed(
        "example.com",
        wire("A", "vpn.example.com", "192.0.2.50")
            .retiring(NEW)
            .json(),
    );
    let machine = at(NEWER, WEB_1);
    let declared = || vec![shared("A", "vpn.example.com", "192.0.2.50")];
    assert_eq!(
        machine.converge(&fake, ENFORCE, declared()),
        Health::Converged
    );
    let vpn = named(&fake, "example.com", "vpn.example.com");
    assert_eq!(vpn[0]["comment"], format!("norn v1 fleet t={NEWER}"));
    assert_eq!(fake.writes().len(), 1);
    assert_eq!(
        machine.converge(&fake, ENFORCE, declared()),
        Health::Converged
    );
    assert_eq!(fake.writes().len(), 1);
}

#[test]
fn a_shared_type_change_is_held_until_the_run_that_deletes_the_old_record() {
    let fake = cloudflare();
    let mut machine = at(OLD, WEB_1);
    machine.rolled_out(OLD, FRESH);
    machine.converge(
        &fake,
        ENFORCE,
        vec![shared("A", "coeus.example.com", "192.0.2.80")],
    );

    machine.revision = Some(NEW);
    let cname = || vec![shared("CNAME", "coeus.example.com", "tunnel.example.net")];
    for _ in 0..2 {
        assert_eq!(machine.converge(&fake, ENFORCE, cname()), Health::Held);
        assert_eq!(
            coeus(&fake),
            ("192.0.2.80".into(), format!("norn v1 fleet t={NEW} delete"))
        );
        let held = lines(&machine);
        assert!(
            held.iter().any(|line| line.starts_with("held: create ")
                && line.contains("waits for the retiring shared record in its way")),
            "{held:#?}"
        );
    }

    machine.rolled_out(NEW, FRESH);
    assert_eq!(machine.converge(&fake, ENFORCE, cname()), Health::Converged);
    assert_eq!(
        coeus(&fake),
        (
            "tunnel.example.net".into(),
            format!("norn v1 fleet t={NEW}")
        )
    );
}

#[test]
fn of_two_machines_creating_at_once_one_creates_and_the_other_has_raced() {
    let fake = cloudflare();
    let first = Arc::new(at(NEW, WEB_1));
    let second = at(NEW, WEB_2);
    let declared = || tunnel("tunnel.example.net");

    let (other, ahead) = (fake.clone(), first.clone());
    fake.before_next_write(move || {
        assert_eq!(
            ahead.converge(&other, ENFORCE, declared()),
            Health::Converged
        );
    });
    assert_eq!(second.converge(&fake, ENFORCE, declared()), Health::Held);
    assert_eq!(
        coeus(&fake),
        (
            "tunnel.example.net".into(),
            format!("norn v1 fleet t={NEW}")
        )
    );
    let raced = lines(&second);
    assert_eq!(raced.len(), 2, "{raced:#?}");
    assert!(
        raced.iter().all(|line| line.starts_with("raced: create ")
            && line.ends_with("another machine wrote it first; the next run reads what is there")),
        "{raced:#?}"
    );
    let writes = fake.writes().len();
    assert_eq!(
        second.converge(&fake, ENFORCE, declared()),
        Health::Converged
    );
    assert_eq!(fake.writes().len(), writes);
}

#[test]
fn a_cname_refused_beside_a_record_that_appeared_meanwhile_has_raced() {
    let fake = cloudflare();
    let machine = at(NEW, WEB_1);
    let other = fake.clone();
    fake.before_next_write(move || {
        other.seed(
            "example.com",
            wire("A", "coeus.example.com", "192.0.2.70").json(),
        );
    });
    let declared = || vec![shared("CNAME", "coeus.example.com", "tunnel.example.net")];
    assert_eq!(machine.converge(&fake, ENFORCE, declared()), Health::Held);
    assert!(lines(&machine)[0].starts_with("raced: create "));
    assert_eq!(
        machine.converge(&fake, ENFORCE, declared()),
        Health::Blocked
    );
}

#[test]
fn a_host_create_refused_the_same_way_is_a_failure() {
    let cases = [
        ("A", "192.0.2.1", "A", "192.0.2.1"),
        ("A", "192.0.2.1", "CNAME", "tunnel.example.net"),
    ];
    for (there, content, kind, target) in cases {
        let fake = cloudflare();
        let machine = Machine::new(WEB_1);
        let other = fake.clone();
        fake.before_next_write(move || {
            other.seed(
                "example.com",
                wire(there, "dup.example.com", content)
                    .marked(&host(WEB_2))
                    .json(),
            );
        });
        let declared = vec![want(&host(WEB_1), kind, "dup.example.com", target)];
        assert_eq!(
            machine.converge(&fake, ENFORCE, declared),
            Health::Failing,
            "{kind}"
        );
        assert!(
            lines(&machine)[0].starts_with("failed: create "),
            "{:#?}",
            lines(&machine)
        );
    }
}

#[test]
fn an_uncommitted_build_holds_the_fleet_and_still_writes_its_own() {
    let fake = cloudflare();
    let machine = Machine::new(WEB_1);
    let mut declared = tunnel("tunnel.example.net");
    declared.push(want(&host(WEB_1), "A", "own.example.com", "192.0.2.10"));
    assert_eq!(machine.converge(&fake, ENFORCE, declared), Health::Held);
    assert!(named(&fake, "example.com", "coeus.example.com").is_empty());
    assert_eq!(named(&fake, "example.com", "own.example.com").len(), 1);
    let held = lines(&machine);
    assert_eq!(held.len(), 2, "{held:#?}");
    assert!(
        held.iter().all(|line| line.ends_with(
            "built from an uncommitted tree, so it has no revision and writes no shared records"
        )),
        "{held:#?}"
    );
}

#[test]
fn an_unknown_or_stale_rollout_holds_the_fleet() {
    for stale in [false, true] {
        let fake = cloudflare();
        let mut machine = at(NEW, WEB_1);
        machine.rolled_out(NEW, Duration::from_secs(2 * 60 * 60));
        if !stale {
            std::fs::remove_file(&machine.rollout.as_ref().unwrap().path).unwrap();
        }
        let health = machine.converge(&fake, ENFORCE, tunnel("tunnel.example.net"));
        assert_eq!(health, Health::Held);
        assert!(fake.writes().is_empty(), "{:#?}", fake.writes());
        let why = if stale {
            "was last checked 2h ago, longer than 30m"
        } else {
            "No such file or directory"
        };
        let held = lines(&machine);
        assert!(
            held.iter().all(|line| line.contains("the fleet's rollout is unknown: ")
                && line.contains(why)),
            "{held:#?}"
        );
    }
}

#[test]
fn an_unread_zone_holds_the_fleet_in_every_zone() {
    let fake = fake::Cloudflare::new(100);
    for zone in ["example.com", "example.org"] {
        fake.seed(zone, wire("TXT", zone, "hello").json());
    }
    let machine = at(NEW, WEB_1);
    let health = machine.converge(&fake, ENFORCE, tunnel("tunnel.example.net"));
    assert_eq!(health, Health::Failing, "example.net is unread");
    assert!(fake.writes().is_empty(), "{:#?}", fake.writes());
    let held: Vec<String> = lines(&machine)
        .into_iter()
        .filter(|line| line.starts_with("held: "))
        .collect();
    assert_eq!(held.len(), 2);
    assert!(
        held.iter()
            .all(|line| line.ends_with("a scope could not be read, so a newer configuration's shared objects may be there unseen")),
        "{held:#?}"
    );
}

#[test]
fn a_report_lists_a_shared_record_once() {
    let fake = cloudflare();
    let machine = at(NEW, WEB_1);
    machine.converge(&fake, ENFORCE, tunnel("tunnel.example.net"));
    let report = machine.report(&fake, tunnel("tunnel.example.net"));
    let shown: Vec<&str> = report
        .lines()
        .filter(|line| line.contains("coeus.example.com"))
        .collect();
    assert_eq!(
        shown.len(),
        0,
        "kept records show only with --verbose: {report}"
    );
    assert!(report.contains(" · 2 kept\n"), "{report}");
    assert!(report.contains(" · 0 others' · "), "{report}");
}

#[test]
fn a_report_says_once_whether_the_fleet_scope_writes() {
    let fake = cloudflare();
    let gate = |machine: &Machine| -> Vec<String> {
        machine
            .report(&fake, tunnel("tunnel.example.net"))
            .lines()
            .filter(|line| line.starts_with("fleet scope: "))
            .map(str::to_owned)
            .collect()
    };

    let mut machine = at(NEW, WEB_1);
    assert_eq!(
        gate(&machine),
        [format!(
            "fleet scope: write at t={NEW}; with no rollout file, no retiring object is deleted"
        )]
    );
    machine.rolled_out(OLD, FRESH);
    assert_eq!(
        gate(&machine),
        [format!(
            "fleet scope: write at t={NEW}; retiring objects stamped up to t={OLD} are deleted"
        )]
    );
    machine.rolled_out(NEWER, FRESH);
    assert_eq!(
        gate(&machine),
        [format!(
            "fleet scope: hold: the fleet has rolled out t={NEWER}, newer than this configuration"
        )]
    );

    let host_only = Machine::new(WEB_1).report(&fake, vec![]);
    assert!(!host_only.contains("fleet scope"), "{host_only}");
}

#[test]
fn fleet_deletes_count_toward_the_machines_delete_cap() {
    let fake = cloudflare();
    let me = host(WEB_1);
    for n in 0..6 {
        let name = format!("h{n}.example.com");
        fake.seed(
            "example.com",
            wire("A", &name, "192.0.2.1").marked(&me).json(),
        );
        let name = format!("f{n}.example.com");
        fake.seed(
            "example.com",
            wire("A", &name, "192.0.2.2").retiring(OLD).json(),
        );
    }
    let mut machine = at(NEW, WEB_1);
    machine.rolled_out(NEW, FRESH);
    assert_eq!(machine.converge(&fake, ENFORCE, vec![]), Health::Blocked);
    assert!(fake.writes().is_empty(), "{:#?}", fake.writes());
    assert_eq!(status::read(&machine.state()).unwrap().counts.withheld, 12);
}
