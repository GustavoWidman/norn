//! the fleet's workers: a script in the account, and routes that run it or
//! bypass it, written by machines that run different revisions at once

use std::{sync::Arc, time::Duration};

use crate::{
    status::{self, Health},
    workers::Module,
};

use super::{
    fake,
    fixture::{WEB_1, WEB_2, host, shared, want, wire},
    run::{Declared, ENFORCE, Machine, cloudflare},
};

const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
const OLD: u64 = 1_794_900_000;
const NEW: u64 = 1_794_942_959;
const NEWER: u64 = 1_795_000_000;
const FRESH: Duration = Duration::from_secs(60);

const SITE: &str = "coeus.example.com/*";
const API: &str = "coeus.example.com/api/*";
const OFFLINE: &str = "coeus-offline";

fn at(revision: u64, host: &'static str) -> Machine {
    let mut machine = Machine::new(host);
    machine.revision = Some(revision);
    machine.account = Some(ACCOUNT);
    machine
}

fn account() -> Arc<fake::Cloudflare> {
    let fake = cloudflare();
    fake.account(ACCOUNT);
    fake
}

fn offline(machine: &Machine, code: &str, routes: &[&str], bypass: &[&str]) -> Declared {
    Declared {
        workers: vec![machine.worker(OFFLINE, code, routes, bypass)],
        ..Declared::default()
    }
}

fn site(machine: &Machine, code: &str) -> Declared {
    offline(machine, code, &[SITE], &[API])
}

/// the tags a script is written with at `revision`, listing `routes` and
/// `bypass`, each in order
fn mark(
    revision: u64,
    retiring: bool,
    code: &str,
    routes: &[&str],
    bypass: &[&str],
) -> Vec<String> {
    let digest = Module::declared(Arc::from(code.as_bytes())).digest;
    let retiring = if retiring { " delete" } else { "" };
    let mut tags = vec![
        format!("norn v1 fleet t={revision}{retiring}"),
        format!("norn v1 module {digest}"),
        "norn v1 date 2026-11-01".to_owned(),
    ];
    for (name, listed) in [("routes", routes), ("bypass", bypass)] {
        if !listed.is_empty() {
            tags.push(format!("norn v1 {name} {}", listed.join(" ")));
        }
    }
    tags
}

fn tags(fake: &fake::Cloudflare) -> Vec<String> {
    let scripts = fake.scripts(ACCOUNT);
    serde_json::from_value(scripts[OFFLINE].listed["tags"].clone()).unwrap()
}

fn routes(fake: &fake::Cloudflare) -> Vec<(String, Option<String>)> {
    fake.routes("example.com")
}

fn route(pattern: &str, script: Option<&str>) -> (String, Option<String>) {
    (pattern.to_owned(), script.map(str::to_owned))
}

fn lines(machine: &Machine) -> Vec<String> {
    status::read(&machine.state()).unwrap().lines
}

#[test]
fn a_worker_is_uploaded_with_its_mark_and_its_routes_follow_it() {
    let fake = account();
    let (first, second) = (at(NEW, WEB_1), at(NEW, WEB_2));
    let code = "export default { fetch() {} }";

    assert_eq!(
        first.converge(&fake, ENFORCE, site(&first, code)),
        Health::Converged
    );
    let scripts = fake.scripts(ACCOUNT);
    assert_eq!(scripts[OFFLINE].module, code.as_bytes());
    assert_eq!(scripts[OFFLINE].listed["compatibility_date"], "2026-11-01");
    assert_eq!(tags(&fake), mark(NEW, false, code, &[SITE], &[API]));
    assert_eq!(
        routes(&fake),
        [route(SITE, Some(OFFLINE)), route(API, None)]
    );
    let verbs: Vec<String> = fake
        .writes()
        .iter()
        .map(|write| write.split(' ').take(2).collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(
        verbs,
        [
            format!("PUT /accounts/{ACCOUNT}/workers/scripts/{OFFLINE}"),
            format!("POST /zones/{:032x}/workers/routes", 0xa000),
            format!("POST /zones/{:032x}/workers/routes", 0xa000),
        ],
        "the script goes up before the routes that run it"
    );

    let writes = fake.writes().len();
    for machine in [&first, &second] {
        assert_eq!(
            machine.converge(&fake, ENFORCE, site(machine, code)),
            Health::Converged
        );
    }
    assert_eq!(fake.writes().len(), writes, "{:#?}", fake.writes());
}

#[test]
fn a_new_revision_uploads_its_code_restamps_for_its_routes_and_the_old_one_stands_down() {
    let fake = account();
    let (old, new) = (at(OLD, WEB_1), at(NEW, WEB_2));
    old.converge(&fake, ENFORCE, site(&old, "v1"));

    assert_eq!(
        new.converge(&fake, ENFORCE, site(&new, "v2")),
        Health::Converged
    );
    assert_eq!(fake.scripts(ACCOUNT)[OFFLINE].module, b"v2");
    assert_eq!(tags(&fake), mark(NEW, false, "v2", &[SITE], &[API]));

    let writes = fake.writes().len();
    assert_eq!(old.converge(&fake, ENFORCE, site(&old, "v1")), Health::Held);
    assert_eq!(fake.writes().len(), writes, "{:#?}", fake.writes());
    assert_eq!(fake.scripts(ACCOUNT)[OFFLINE].module, b"v2");
    assert!(
        lines(&old).iter().all(|line| line.starts_with("held: ")
            && line.ends_with(&format!(
                "a shared object carries t={NEW}, newer than this configuration, whose machines write the shared objects"
            ))),
        "{:#?}",
        lines(&old)
    );

    let newer = at(NEWER, WEB_1);
    let live = "coeus.example.com/live/*";
    let declared = || offline(&newer, "v2", &[SITE], &[API, live]);
    assert_eq!(
        newer.converge(&fake, ENFORCE, declared()),
        Health::Converged
    );
    let sent = fake.writes().split_off(writes);
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert!(
        sent[0].starts_with(&format!(
            "PATCH /accounts/{ACCOUNT}/workers/scripts/{OFFLINE}/script-settings "
        )),
        "new routes alone restamp the script without uploading it: {sent:#?}"
    );
    assert!(
        sent[1].starts_with("POST ") && sent[1].contains(live),
        "{sent:#?}"
    );
    assert_eq!(tags(&fake), mark(NEWER, false, "v2", &[SITE], &[API, live]));
    assert_eq!(fake.scripts(ACCOUNT)[OFFLINE].module, b"v2");

    let writes = fake.writes().len();
    assert_eq!(
        new.converge(&fake, ENFORCE, site(&new, "v2")),
        Health::Held,
        "the restamp tells the configuration before it to leave the new route be"
    );
    assert_eq!(fake.writes().len(), writes, "{:#?}", fake.writes());
}

#[test]
fn a_dropped_worker_retires_then_its_bypasses_go_then_its_script_with_its_routes() {
    let fake = account();
    let theirs = "coeus.example.com/static/*";
    fake.seed_route("example.com", theirs, None);
    let mut first = at(OLD, WEB_1);
    first.rolled_out(OLD, FRESH);
    first.converge(&fake, ENFORCE, site(&first, "v1"));

    first.revision = Some(NEW);
    assert_eq!(
        first.converge(&fake, ENFORCE, Declared::default()),
        Health::Held
    );
    assert_eq!(tags(&fake), mark(NEW, true, "v1", &[SITE], &[API]));
    assert_eq!(
        routes(&fake),
        [
            route(SITE, Some(OFFLINE)),
            route(API, None),
            route(theirs, None)
        ]
    );
    for pattern in [SITE, API] {
        assert!(
            lines(&first).iter().any(|line| line.starts_with(&format!(
                "held: delete worker route example.com: fleet {pattern} "
            )) && line
                .contains(&format!("it is deleted once the whole fleet runs t={NEW}"))),
            "{:#?}",
            lines(&first)
        );
    }

    let mut second = at(OLD, WEB_2);
    second.rolled_out(OLD, FRESH);
    let writes = fake.writes().len();
    assert_eq!(
        second.converge(&fake, ENFORCE, site(&second, "v1")),
        Health::Held
    );
    assert_eq!(fake.writes().len(), writes, "{:#?}", fake.writes());

    first.rolled_out(NEW, FRESH);
    assert_eq!(
        first.converge(&fake, ENFORCE, Declared::default()),
        Health::Held
    );
    assert_eq!(routes(&fake), [route(theirs, None)]);
    assert!(fake.scripts(ACCOUNT).contains_key(OFFLINE));
    assert!(
        lines(&first)
            .iter()
            .any(|line| line.starts_with("held: delete worker script ")
                && line.ends_with("so it is deleted in the run after its bypasses are")),
        "{:#?}",
        lines(&first)
    );

    assert_eq!(
        first.converge(&fake, ENFORCE, Declared::default()),
        Health::Converged
    );
    assert!(fake.scripts(ACCOUNT).is_empty());
    assert_eq!(routes(&fake), [route(theirs, None)]);
}

#[test]
fn a_dropped_worker_without_bypasses_goes_with_its_routes_in_one_run() {
    let fake = account();
    let mut machine = at(OLD, WEB_1);
    machine.converge(&fake, ENFORCE, offline(&machine, "v1", &[SITE], &[]));
    machine.revision = Some(NEW);
    machine.converge(&fake, ENFORCE, Declared::default());

    machine.rolled_out(NEW, FRESH);
    assert_eq!(
        machine.converge(&fake, ENFORCE, Declared::default()),
        Health::Converged
    );
    assert!(fake.scripts(ACCOUNT).is_empty());
    assert!(routes(&fake).is_empty());
}

#[test]
fn a_dropped_route_stays_listed_until_the_rollout_deletes_it() {
    let fake = account();
    let www = "www.coeus.example.com/*";
    let mut machine = at(OLD, WEB_1);
    machine.rolled_out(OLD, FRESH);
    machine.converge(&fake, ENFORCE, offline(&machine, "v1", &[SITE, www], &[]));

    machine.revision = Some(NEW);
    let dropped = |machine: &Machine| offline(machine, "v1", &[SITE], &[]);
    let writes = fake.writes().len();
    assert_eq!(
        machine.converge(&fake, ENFORCE, dropped(&machine)),
        Health::Held
    );
    assert_eq!(fake.writes().len(), writes, "{:#?}", fake.writes());
    assert_eq!(tags(&fake), mark(OLD, false, "v1", &[SITE, www], &[]));

    machine.rolled_out(NEW, FRESH);
    assert_eq!(
        machine.converge(&fake, ENFORCE, dropped(&machine)),
        Health::Converged
    );
    assert_eq!(routes(&fake), [route(SITE, Some(OFFLINE))]);
    assert_eq!(
        machine.converge(&fake, ENFORCE, dropped(&machine)),
        Health::Converged
    );
    assert_eq!(
        tags(&fake),
        mark(NEW, false, "v1", &[SITE], &[]),
        "once the route is gone, the script stops listing it"
    );
    let writes = fake.writes().len();
    machine.converge(&fake, ENFORCE, dropped(&machine));
    assert_eq!(fake.writes().len(), writes, "{:#?}", fake.writes());
}

#[test]
fn a_persons_route_that_runs_the_fleet_script_is_never_deleted() {
    let fake = account();
    let blog = "blog.example.com/*";
    let mut machine = at(OLD, WEB_1);
    machine.converge(&fake, ENFORCE, site(&machine, "v1"));
    fake.seed_route("example.com", blog, Some(OFFLINE));

    machine.revision = Some(NEW);
    machine.rolled_out(NEW, FRESH);
    assert_eq!(
        machine.converge(&fake, ENFORCE, offline(&machine, "v1", &[SITE], &[])),
        Health::Converged
    );
    assert_eq!(
        routes(&fake),
        [route(blog, Some(OFFLINE)), route(SITE, Some(OFFLINE))]
    );
    assert!(
        fake.writes().iter().all(|write| !write.contains(blog)),
        "{:#?}",
        fake.writes()
    );
    let report = machine.report(&fake, offline(&machine, "v1", &[SITE], &[]));
    assert!(
        report
            .lines()
            .any(|line| line.trim_start().starts_with("untouched") && line.contains(blog)),
        "{report}"
    );
}

#[test]
fn a_persons_bypass_at_a_declared_pattern_is_never_rewritten_or_claimed() {
    let fake = account();
    fake.seed_route("example.com", SITE, None);
    fake.seed_route("example.com", API, None);
    let machine = at(NEW, WEB_1);
    for _ in 0..2 {
        assert_eq!(
            machine.converge(&fake, ENFORCE, site(&machine, "v1")),
            Health::Blocked
        );
        assert_eq!(routes(&fake), [route(SITE, None), route(API, None)]);
        assert_eq!(
            tags(&fake),
            mark(NEW, false, "v1", &[], &[]),
            "the script lists no pattern a person's route holds"
        );
    }
    let writes = fake.writes();
    assert_eq!(writes.len(), 1, "only the script goes up: {writes:#?}");
    let blocked: Vec<String> = lines(&machine)
        .into_iter()
        .filter(|line| line.starts_with("blocked: worker route "))
        .collect();
    assert_eq!(blocked.len(), 2, "{blocked:#?}");
}

#[test]
fn a_route_that_runs_a_script_norn_does_not_own_is_left_alone() {
    let fake = account();
    fake.seed_script(ACCOUNT, "theirs", &["team-web"]);
    fake.seed_route("example.com", "blog.example.com/*", Some("theirs"));
    fake.seed_route("example.com", SITE, Some("theirs"));
    let machine = at(NEW, WEB_1);

    assert_eq!(
        machine.converge(&fake, ENFORCE, offline(&machine, "v1", &[SITE], &[])),
        Health::Blocked
    );
    assert_eq!(
        routes(&fake),
        [
            route("blog.example.com/*", Some("theirs")),
            route(SITE, Some("theirs"))
        ]
    );
    let writes = fake.writes();
    assert_eq!(writes.len(), 1, "only the script goes up: {writes:#?}");
    assert_eq!(tags(&fake), mark(NEW, false, "v1", &[], &[]));
    assert_eq!(
        fake.scripts(ACCOUNT)["theirs"].listed["tags"],
        serde_json::json!(["team-web"])
    );
    assert_eq!(
        lines(&machine),
        [format!(
            "blocked: worker route example.com: fleet {SITE} runs {OFFLINE}: an object norn does not own sits in this slot, and norn cannot claim one it did not write; remove it first; there now: {SITE} runs theirs"
        )]
    );
}

#[test]
fn a_script_norn_does_not_own_blocks_the_worker_and_no_route_runs_it() {
    let fake = account();
    fake.seed_script(ACCOUNT, OFFLINE, &[]);
    let machine = at(NEW, WEB_1);
    assert_eq!(
        machine.converge(&fake, ENFORCE, site(&machine, "v1")),
        Health::Blocked
    );
    assert!(
        fake.writes().iter().all(|write| !write.contains(OFFLINE)),
        "{:#?}",
        fake.writes()
    );
    assert_eq!(routes(&fake), [route(API, None)]);
    let lines = lines(&machine);
    assert!(
        lines.iter().any(|line| line.starts_with(&format!(
            "blocked: worker script {ACCOUNT}: fleet {OFFLINE} "
        ))),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line.ends_with(&format!(
            "it runs {OFFLINE}, which is not the fleet's yet; it waits for that script"
        ))),
        "{lines:#?}"
    );
}

#[test]
fn of_two_machines_uploading_at_once_both_land_and_the_second_route_create_has_raced() {
    let fake = account();
    let first = Arc::new(at(NEW, WEB_1));
    let second = at(NEW, WEB_2);

    let (other, ahead) = (fake.clone(), first.clone());
    fake.before_next_write(move || {
        let declared = site(&ahead, "v1");
        assert_eq!(ahead.converge(&other, ENFORCE, declared), Health::Converged);
    });
    assert_eq!(
        second.converge(&fake, ENFORCE, site(&second, "v1")),
        Health::Held
    );
    assert_eq!(tags(&fake), mark(NEW, false, "v1", &[SITE], &[API]));
    assert_eq!(
        routes(&fake),
        [route(SITE, Some(OFFLINE)), route(API, None)]
    );
    let raced = lines(&second);
    assert_eq!(raced.len(), 2, "{raced:#?}");
    assert!(
        raced
            .iter()
            .all(|line| line.starts_with("raced: create worker route ")),
        "{raced:#?}"
    );

    let writes = fake.writes().len();
    assert_eq!(
        second.converge(&fake, ENFORCE, site(&second, "v1")),
        Health::Converged
    );
    assert_eq!(fake.writes().len(), writes);
}

#[test]
fn a_route_a_person_makes_mid_run_fails_the_create_rather_than_racing_it() {
    let fake = account();
    let machine = at(NEW, WEB_1);
    let other = fake.clone();
    fake.before_next_write(move || other.seed_route("example.com", SITE, Some("theirs")));
    assert_eq!(
        machine.converge(&fake, ENFORCE, offline(&machine, "v1", &[SITE], &[])),
        Health::Failing
    );
    assert_eq!(routes(&fake), [route(SITE, Some("theirs"))]);
    let lines = lines(&machine);
    assert!(
        lines.iter().any(|line| line.starts_with(&format!(
            "failed: create worker route example.com: fleet {SITE} "
        )) && line.contains("10020")),
        "{lines:#?}"
    );
}

#[test]
fn a_route_waits_while_the_tags_that_list_it_fail_to_write() {
    let fake = account();
    let old = at(OLD, WEB_1);
    old.converge(&fake, ENFORCE, offline(&old, "v1", &[SITE], &[]));
    let mine = mark(OLD, false, "v1", &[SITE], &[]);
    let theirs: Vec<String> = (0..6).map(|n| format!("team-{n}")).collect();
    fake.retag(ACCOUNT, OFFLINE, &[mine, theirs.clone()].concat());

    let new = at(NEW, WEB_1);
    assert_eq!(
        new.converge(&fake, ENFORCE, site(&new, "v1")),
        Health::Failing
    );
    assert_eq!(routes(&fake), [route(SITE, Some(OFFLINE))]);
    let lines = lines(&new);
    assert!(
        lines.iter().any(
            |line| line.starts_with("failed: update worker script ") && line.contains("100103")
        ),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with(&format!(
                "held: create worker route example.com: fleet {API} "
            )) && line.ends_with("writing them failed, so it waits for the next run")),
        "{lines:#?}"
    );

    fake.retag(
        ACCOUNT,
        OFFLINE,
        &[mark(OLD, false, "v1", &[SITE], &[]), theirs[..5].to_vec()].concat(),
    );
    assert_eq!(
        new.converge(&fake, ENFORCE, site(&new, "v1")),
        Health::Converged
    );
    assert_eq!(
        routes(&fake),
        [route(SITE, Some(OFFLINE)), route(API, None)]
    );
    assert_eq!(
        tags(&fake),
        [
            mark(NEW, false, "v1", &[SITE], &[API]),
            theirs[..5].to_vec()
        ]
        .concat()
    );
}

#[test]
fn an_unreadable_account_holds_every_shared_object() {
    let fake = cloudflare();
    let machine = at(NEW, WEB_1);
    let mut declared = site(&machine, "v1");
    declared.dns = vec![
        shared("CNAME", "coeus.example.com", "tunnel.example.net"),
        want(&host(WEB_1), "A", "own.example.com", "192.0.2.10"),
    ];
    assert_eq!(machine.converge(&fake, ENFORCE, declared), Health::Failing);
    let writes = fake.writes();
    assert_eq!(writes.len(), 1, "{writes:#?}");
    assert!(
        writes[0].contains("own.example.com"),
        "its own records still go: {writes:#?}"
    );
    let status = status::read(&machine.state()).unwrap();
    assert_eq!(
        status.counts.unread, 4,
        "the account, and the routes of 3 zones"
    );
    let held: Vec<&String> = status
        .lines
        .iter()
        .filter(|line| line.starts_with("held: "))
        .collect();
    assert_eq!(held.len(), 1, "{:#?}", status.lines);
    assert!(
        held[0].ends_with("a scope could not be read, so a newer configuration's shared objects may be there unseen"),
        "{held:#?}"
    );
}

#[test]
fn a_bypass_declared_where_the_worker_ran_takes_its_route_over() {
    let fake = account();
    let machine = at(OLD, WEB_1);
    machine.converge(&fake, ENFORCE, offline(&machine, "v1", &[SITE, API], &[]));
    let newer = at(NEW, WEB_1);
    assert_eq!(
        newer.converge(&fake, ENFORCE, site(&newer, "v1")),
        Health::Converged
    );
    assert_eq!(
        routes(&fake),
        [route(SITE, Some(OFFLINE)), route(API, None)]
    );
    assert!(
        fake.writes()
            .iter()
            .any(|write| write.starts_with("PUT /zones/")
                && write.ends_with(&format!(r#"{{"pattern":"{API}"}}"#))),
        "{:#?}",
        fake.writes()
    );
    newer.converge(&fake, ENFORCE, site(&newer, "v1"));
    assert_eq!(tags(&fake), mark(NEW, false, "v1", &[SITE], &[API]));
    let writes = fake.writes().len();
    assert_eq!(
        newer.converge(&fake, ENFORCE, site(&newer, "v1")),
        Health::Converged
    );
    assert_eq!(fake.writes().len(), writes, "{:#?}", fake.writes());
}

#[test]
fn a_report_reads_workers_and_writes_nothing() {
    let fake = account();
    let machine = at(NEW, WEB_1);
    let report = machine.report(&fake, site(&machine, "v1"));
    assert!(fake.writes().is_empty(), "{:#?}", fake.writes());
    let rows = |kind: &str| -> Vec<String> {
        report
            .split(&format!("\n{kind}\n"))
            .nth(1)
            .unwrap_or_default()
            .lines()
            .take_while(|line| !line.is_empty())
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    };
    let script = rows(&format!("worker script · {ACCOUNT}"));
    assert!(
        matches!(script.as_slice(), [row] if row.starts_with(&format!("create fleet {OFFLINE} module "))),
        "{report}"
    );
    assert_eq!(
        rows("worker route · example.com"),
        [
            format!("create fleet {SITE} runs {OFFLINE}"),
            format!("create fleet {API} runs no worker"),
        ],
        "{report}"
    );
}

#[test]
fn without_an_account_norn_reads_no_workers() {
    let fake = account();
    fake.seed_route("example.com", SITE, Some("theirs"));
    let machine = Machine::new(WEB_1);
    machine.converge(
        &fake,
        ENFORCE,
        vec![want(&host(WEB_1), "A", "own.example.com", "192.0.2.10")],
    );
    let report = machine.report(&fake, Declared::default());
    assert!(!report.contains("worker"), "{report}");
}

#[test]
fn deletes_of_every_kind_count_toward_one_cap() {
    let fake = account();
    let mut machine = at(OLD, WEB_1);
    machine.rolled_out(OLD, FRESH);
    machine.converge(&fake, ENFORCE, offline(&machine, "v1", &[SITE], &[]));
    let me = host(WEB_1);
    for n in 0..10 {
        let name = format!("h{n}.example.com");
        fake.seed(
            "example.com",
            wire("A", &name, "192.0.2.1").marked(&me).json(),
        );
    }
    machine.revision = Some(NEW);
    machine.rolled_out(NEW, FRESH);
    let dropped = |machine: &Machine| offline(machine, "v1", &[], &[]);
    assert_eq!(
        machine.converge(&fake, ENFORCE, dropped(&machine)),
        Health::Blocked
    );
    assert_eq!(routes(&fake), [route(SITE, Some(OFFLINE))]);
    assert_eq!(status::read(&machine.state()).unwrap().counts.withheld, 11);
    assert_eq!(
        machine.converge_deleting(&fake, ENFORCE, dropped(&machine), 11),
        Health::Converged
    );
    assert!(routes(&fake).is_empty());
}

#[test]
fn a_delete_held_for_another_kind_does_not_count_toward_the_cap() {
    let fake = account();
    let mut machine = at(OLD, WEB_1);
    machine.converge(&fake, ENFORCE, site(&machine, "v1"));
    machine.revision = Some(NEW);
    machine.converge(&fake, ENFORCE, Declared::default());

    machine.rolled_out(NEW, FRESH);
    assert_eq!(
        machine.converge_deleting(&fake, ENFORCE, Declared::default(), 2),
        Health::Held,
        "the route and the bypass go, and the script waits a run for the bypass"
    );
    assert!(routes(&fake).is_empty());
    assert!(fake.scripts(ACCOUNT).contains_key(OFFLINE));
    assert_eq!(
        machine.converge_deleting(&fake, ENFORCE, Declared::default(), 1),
        Health::Converged
    );
    assert!(fake.scripts(ACCOUNT).is_empty());
}
