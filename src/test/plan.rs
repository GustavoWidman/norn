use crate::{
    dns::{Dns, Ttl},
    kind::{Live, Ownership, Plan, Step, Want, Why},
    model::Zone,
    plan::{MAX_DELETES, plan},
};

use super::fixture::{
    WEB_1, WEB_2, apply, baseline, declare, host, mx, proxied, want, web_1, web_2, wire, zone,
};

fn plan_zone(machine: &str, wants: &[Want<Dns>], zone: &Zone, live: Vec<Live<Dns>>) -> Plan<Dns> {
    let mut budget = MAX_DELETES;
    let wants = wants
        .iter()
        .filter(|want| want.scope == *zone)
        .cloned()
        .collect();
    plan(&host(machine), wants, live, &mut budget)
}

fn lines(plan: &Plan<Dns>) -> Vec<String> {
    plan.steps
        .iter()
        .map(|step| {
            format!(
                "{} {} {} {}",
                step.verb(),
                step.owner(),
                step.key(),
                step.describe()
            )
        })
        .collect()
}

fn verbs(plan: &Plan<Dns>) -> Vec<&'static str> {
    plan.steps.iter().map(Step::verb).collect()
}

fn assert_writes_only_its_own(machine: &str, plan: &Plan<Dns>) {
    let me = host(machine);
    for step in plan
        .steps
        .iter()
        .chain(plan.withheld.iter().map(|withheld| &withheld.step))
    {
        match step {
            Step::Adopt { live, .. } => {
                assert_eq!(
                    live.ownership,
                    Ownership::Unmarked { annotated: false },
                    "{step:?}"
                );
            }
            Step::Update { live, .. } | Step::Delete { live, .. } => match &live.ownership {
                Ownership::Marked(owner) => assert_eq!(*owner, me, "{step:?}"),
                other => panic!("{other:?} in {step:?}"),
            },
            Step::Create { .. } => {}
        }
        assert_eq!(*step.owner(), me);
    }
}

#[test]
fn the_baseline_plans_23_adopts_and_leaves_9_untouched() {
    let mut adopts = Vec::new();
    let mut untouched_by_both = 0;
    for (zone, live) in baseline() {
        let one = plan_zone(WEB_1, &web_1(), &zone, live.clone());
        let two = plan_zone(WEB_2, &web_2(), &zone, live.clone());
        for plan in [&one, &two] {
            assert!(
                plan.steps.iter().all(|step| step.verb() == "adopt"),
                "{:?}",
                lines(plan)
            );
            assert!(plan.blocked.is_empty() && plan.withheld.is_empty() && plan.kept.is_empty());
            adopts.extend(lines(plan));
        }
        assert_writes_only_its_own(WEB_1, &one);
        assert_writes_only_its_own(WEB_2, &two);
        untouched_by_both += one
            .untouched
            .iter()
            .filter(|object| {
                two.untouched
                    .iter()
                    .any(|other| other.handle == object.handle)
            })
            .count();
    }
    assert_eq!(
        adopts
            .iter()
            .filter(|line| line.contains(" web-1 "))
            .count(),
        5
    );
    assert_eq!(
        adopts
            .iter()
            .filter(|line| line.contains(" web-2 "))
            .count(),
        18
    );
    assert_eq!(untouched_by_both, 9);
}

#[test]
fn a_second_plan_over_the_adopted_baseline_has_no_steps() {
    for (zone, live) in baseline() {
        let first = plan_zone(WEB_1, &web_1(), &zone, live.clone());
        let live = apply(live, &first.steps);
        let second = plan_zone(WEB_2, &web_2(), &zone, live.clone());
        let live = apply(live, &second.steps);
        for (machine, wants) in [(WEB_1, web_1()), (WEB_2, web_2())] {
            let again = plan_zone(machine, &wants, &zone, live.clone());
            assert!(again.steps.is_empty(), "{zone}: {:?}", lines(&again));
            assert!(again.blocked.is_empty());
        }
    }
}

#[test]
fn applying_any_plan_converges_in_one_run() {
    let me = host(WEB_1);
    let other = host(WEB_2);
    let com = zone("example.com");
    let live: Vec<Live<Dns>> = [
        wire("A", "moved.example.com", "192.0.2.1").marked(&me),
        wire("A", "gone.example.com", "192.0.2.1").marked(&me),
        wire("A", "flip.example.com", "192.0.2.1").marked(&me),
        wire("A", "pool.example.com", "192.0.2.1").marked(&me),
        wire("A", "pool.example.com", "192.0.2.2").marked(&me),
        wire("TXT", "example.com", "\"v=spf1 -all\"").marked(&me),
        wire("A", "theirs.example.com", "192.0.2.9").marked(&other),
        wire("A", "loose.example.com", "192.0.2.5"),
        wire("TXT", "_acme-challenge.example.com", "\"token\"").ttl(120),
    ]
    .iter()
    .map(|record| record.live())
    .collect();
    let wants = vec![
        want(&me, "A", "moved.example.com", "192.0.2.2"),
        proxied(&me, "CNAME", "flip.example.com", "example.com"),
        want(&me, "A", "pool.example.com", "192.0.2.2"),
        want(&me, "A", "pool.example.com", "192.0.2.3"),
        want(&me, "A", "pool.example.com", "192.0.2.4"),
        want(&me, "TXT", "example.com", "v=spf1 -all"),
        want(&me, "A", "loose.example.com", "192.0.2.5"),
        want(&me, "A", "new.example.com", "192.0.2.7"),
        declare(
            &me,
            "TXT",
            "new.example.com",
            "hello",
            None,
            Ttl::Seconds(300),
            None,
        ),
        proxied(&me, "A", "web.example.com", "192.0.2.8"),
    ];

    let first = plan_zone(WEB_1, &wants, &com, live.clone());
    assert_writes_only_its_own(WEB_1, &first);
    assert!(first.blocked.is_empty(), "{:?}", first.blocked);
    let live = apply(live, &first.steps);
    let second = plan_zone(WEB_1, &wants, &com, live);
    assert!(second.steps.is_empty(), "{:?}", lines(&second));
    assert_eq!(second.kept.len(), wants.len());
}

#[test]
fn nothing_unmarked_foreign_or_unreadable_is_ever_written() {
    let me = host(WEB_1);
    let other = host(WEB_2);
    let com = zone("example.com");
    let live: Vec<Live<Dns>> = [
        wire("A", "a.example.com", "192.0.2.1"),
        wire("A", "b.example.com", "192.0.2.1").comment("kept by hand"),
        wire("A", "c.example.com", "192.0.2.1").marked(&other),
        wire("A", "d.example.com", "192.0.2.1").comment("norn v2 host=web-1"),
        wire("A", "e.example.com", "192.0.2.1").proxied(),
        wire("SRV", "_sip._tcp.example.com", "0 5 5060 sip.example.com").marked(&me),
        wire("TXT", "example.com", "v=spf1 ~all"),
        wire("CNAME", "f.example.com", "example.com"),
    ]
    .iter()
    .map(|record| record.live())
    .collect();
    let wants = vec![
        want(&me, "A", "a.example.com", "192.0.2.2"),
        want(&me, "A", "b.example.com", "192.0.2.1"),
        want(&me, "A", "c.example.com", "192.0.2.1"),
        want(&me, "A", "d.example.com", "192.0.2.1"),
        want(&me, "A", "e.example.com", "192.0.2.1"),
        want(&me, "TXT", "example.com", "v=spf1 -all"),
        want(&me, "A", "f.example.com", "192.0.2.1"),
    ];

    let plan = plan_zone(WEB_1, &wants, &com, live);
    assert!(plan.steps.is_empty(), "{:?}", lines(&plan));
    let why: Vec<(String, Why)> = plan
        .blocked
        .iter()
        .map(|blocked| (blocked.want.name.to_string(), blocked.why.clone()))
        .collect();
    assert_eq!(
        why,
        [
            ("example.com".into(), Why::Occupied),
            ("a.example.com".into(), Why::Occupied),
            ("b.example.com".into(), Why::Annotated),
            ("c.example.com".into(), Why::Owned(other)),
            ("d.example.com".into(), Why::Unreadable),
            ("e.example.com".into(), Why::Differs),
            ("f.example.com".into(), Why::Excludes),
        ]
    );
    assert_eq!(
        plan.foreign.len(),
        3,
        "another's mark, an unreadable mark and a marked SRV"
    );
}

#[test]
fn the_delete_cap_holds_across_the_scopes_of_a_run() {
    let me = host(WEB_1);
    let mut budget = MAX_DELETES;
    let mut deleted = 0;
    let mut withheld = 0;
    for zone_name in ["example.com", "example.net"] {
        let live: Vec<Live<Dns>> = (0..8)
            .map(|n| {
                wire("A", &format!("h{n}.{zone_name}"), "192.0.2.1")
                    .marked(&me)
                    .live()
            })
            .collect();
        let plan = plan(&me, vec![], live, &mut budget);
        deleted += plan
            .steps
            .iter()
            .filter(|step| step.verb() == "delete")
            .count();
        withheld += plan.withheld.len();
        assert!(
            plan.withheld
                .iter()
                .all(|withheld| withheld.step.verb() == "delete")
        );
    }
    assert_eq!(
        (deleted, withheld, budget),
        (MAX_DELETES, 16 - MAX_DELETES, 0)
    );
}

#[test]
fn a_create_is_blocked_beside_an_unmarked_record_in_its_slot() {
    let me = host(WEB_2);
    let com = zone("example.com");
    let live = vec![wire("TXT", "example.com", "v=spf1 include:spf.example.net ~all").live()];
    let wants = vec![want(
        &me,
        "TXT",
        "example.com",
        "v=spf1 include:spf.example.net -all",
    )];
    let plan = plan_zone(WEB_2, &wants, &com, live);
    assert!(plan.steps.is_empty());
    assert_eq!(plan.blocked[0].why, Why::Occupied);
}

#[test]
fn a_cname_shares_its_name_with_nothing() {
    let me = host(WEB_1);
    let com = zone("example.com");

    let live = vec![wire("A", "app.example.com", "192.0.2.1").live()];
    let plan = plan_zone(
        WEB_1,
        &[proxied(&me, "CNAME", "app.example.com", "example.com")],
        &com,
        live,
    );
    assert!(plan.steps.is_empty());
    assert_eq!(plan.blocked[0].why, Why::Excludes);

    let live = vec![wire("CNAME", "app.example.com", "example.com").live()];
    let plan = plan_zone(
        WEB_1,
        &[want(&me, "TXT", "app.example.com", "hello")],
        &com,
        live,
    );
    assert!(plan.steps.is_empty());
    assert_eq!(plan.blocked[0].why, Why::Excludes);

    let live = vec![wire("A", "app.example.com", "192.0.2.1").marked(&me).live()];
    let plan = plan_zone(
        WEB_1,
        &[proxied(&me, "CNAME", "app.example.com", "example.com")],
        &com,
        live,
    );
    assert_eq!(verbs(&plan), ["delete", "create"]);
}

#[test]
fn quoted_and_bare_txt_with_one_text_are_one_record() {
    let me = host(WEB_2);
    let com = zone("example.com");
    let live = vec![wire("TXT", "_dmarc.example.com", "\"v=DMARC1; p=none\"").live()];
    let wants = [want(&me, "TXT", "_dmarc.example.com", "v=DMARC1; p=none")];
    let plan = plan_zone(WEB_2, &wants, &com, live);
    assert_eq!(verbs(&plan), ["adopt"]);
    let marked = vec![
        wire("TXT", "_dmarc.example.com", "\"v=DMARC1; p=none\"")
            .marked(&me)
            .live(),
    ];
    assert!(plan_zone(WEB_2, &wants, &com, marked).steps.is_empty());
}

#[test]
fn a_proxied_record_reported_with_ttl_1_plans_nothing() {
    let me = host(WEB_2);
    let com = zone("example.com");
    let live = vec![
        wire("A", "blog.example.com", "192.0.2.20")
            .proxied()
            .ttl(1)
            .marked(&me)
            .live(),
    ];
    let plan = plan_zone(
        WEB_2,
        &[proxied(&me, "A", "blog.example.com", "192.0.2.20")],
        &com,
        live,
    );
    assert!(plan.steps.is_empty());
    assert_eq!(plan.kept.len(), 1);
}

#[test]
fn an_address_change_is_one_update() {
    let me = host(WEB_1);
    let com = zone("example.com");
    let live = vec![
        wire("A", "cache.example.com", "192.0.2.10")
            .marked(&me)
            .live(),
    ];
    let plan = plan_zone(
        WEB_1,
        &[want(&me, "A", "cache.example.com", "192.0.2.11")],
        &com,
        live,
    );
    assert_eq!(
        lines(&plan),
        ["update web-1 A      cache.example.com 192.0.2.10  dns only -> 192.0.2.11  dns only"]
    );
}

#[test]
fn duplicates_keep_the_oldest() {
    let me = host(WEB_1);
    let com = zone("example.com");
    let older = wire("A", "app.example.com", "192.0.2.1").marked(&me).live();
    let younger = wire("A", "app.example.com", "192.0.2.1").marked(&me).live();
    let wants = [want(&me, "A", "app.example.com", "192.0.2.1")];
    let plan = plan_zone(WEB_1, &wants, &com, vec![younger.clone(), older]);
    match plan.steps.as_slice() {
        [Step::Delete { live, .. }] => assert_eq!(live.handle, younger.handle),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_plan_does_not_depend_on_input_order() {
    for (zone, live) in baseline() {
        let mut wants = web_2();
        wants.push(mx(&host(WEB_2), "example.org", "mail.example.com", 5));
        let forward = lines(&plan_zone(WEB_2, &wants, &zone, live.clone()));
        wants.reverse();
        let mut reversed = live;
        reversed.reverse();
        assert_eq!(lines(&plan_zone(WEB_2, &wants, &zone, reversed)), forward);
    }
}

#[test]
fn a_name_is_deleted_right_before_it_is_created() {
    let me = host(WEB_1);
    let com = zone("example.com");
    let live: Vec<Live<Dns>> = [
        wire("A", "app.example.com", "192.0.2.1").marked(&me),
        wire("A", "old.example.com", "192.0.2.1").marked(&me),
        wire("A", "cache.example.com", "192.0.2.1").marked(&me),
        wire("A", "loose.example.com", "192.0.2.5"),
    ]
    .iter()
    .map(|record| record.live())
    .collect();
    let wants = vec![
        want(&me, "CNAME", "app.example.com", "example.com"),
        want(&me, "A", "cache.example.com", "192.0.2.2"),
        want(&me, "A", "loose.example.com", "192.0.2.5"),
    ];
    let plan = plan_zone(WEB_1, &wants, &com, live);
    let steps: Vec<String> = plan
        .steps
        .iter()
        .map(|step| format!("{} {}", step.verb(), step.key()))
        .collect();
    assert_eq!(
        steps,
        [
            "adopt A      loose.example.com",
            "update A      cache.example.com",
            "delete A      app.example.com",
            "create CNAME  app.example.com",
            "delete A      old.example.com",
        ]
    );
}
