use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use chrono::{DateTime, TimeDelta, Utc};
use gjallar::Receiver;
use serde_json::{Value, json};

use crate::{
    push::{Notice, PushSettings, Turn, message, notice, settle},
    run::{Cloudflare, converge_with},
    status::{self, Health, Status, Summary},
};

use super::{
    fixture::{WEB_1, web_1},
    run::{ENFORCE, MAX_DELETES, Machine, cloudflare, timing},
};

const AFTER: Duration = Duration::from_secs(30 * 60);
const SWEEP: i64 = 15;

fn start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
        .unwrap()
        .to_utc()
}

fn summary(health: Health) -> Summary {
    match health {
        Health::Converged => Summary::of(&[]),
        Health::Failing => Summary::problem(
            health,
            "unread: worker route example.com: cloudflare answered 403: Authentication error"
                .into(),
        ),
        Health::Blocked => Summary::problem(
            health,
            "blocked: dns record example.com grafana.example.com: an object norn does not own sits in this slot"
                .into(),
        ),
        Health::Skipped => {
            Summary::problem(health, "no cloudflare token yet; skipping this run".into())
        }
        Health::Held => Summary::problem(
            health,
            "held: create dns record example.com tunnel.example.com: the fleet runs a newer configuration"
                .into(),
        ),
    }
}

/// the notices a run of each `(minute, health)` sends, as
/// `minute: title / body`. `heard` says whether the devices took the notice
/// sent at that minute
fn notices(runs: &[(i64, Health)], heard: impl Fn(i64) -> bool) -> Vec<String> {
    let mut previous: Option<Status> = None;
    let mut sent = Vec::new();
    for &(minute, health) in runs {
        let now = start() + TimeDelta::minutes(minute);
        let mut current = Status::following(previous.as_ref(), &summary(health), now);
        if let Some(notice) = notice(previous.as_ref(), &current, AFTER, "oracle-1") {
            sent.push(format!(
                "{minute}: {} / {}",
                notice.title,
                notice.body.replace('\n', " | ")
            ));
            if heard(minute) {
                settle(&mut current, &notice);
            }
        }
        previous = Some(current);
    }
    sent
}

fn every_sweep(health: Health, until: i64) -> Vec<(i64, Health)> {
    (0..=until / SWEEP).map(|n| (n * SWEEP, health)).collect()
}

fn always(_: i64) -> bool {
    true
}

#[test]
fn one_bad_run_says_nothing() {
    assert!(notices(&[(0, Health::Failing), (15, Health::Converged)], always).is_empty());
}

#[test]
fn two_bad_runs_still_say_nothing() {
    let runs = [
        (0, Health::Failing),
        (15, Health::Failing),
        (29, Health::Failing),
        (40, Health::Converged),
    ];
    assert!(notices(&runs, always).is_empty());
}

#[test]
fn thirty_minutes_of_trouble_says_so_once_however_long_it_lasts() {
    assert_eq!(
        notices(&every_sweep(Health::Failing, 24 * 60), always),
        [
            "30: norn failing on oracle-1 / unread: worker route example.com: cloudflare answered 403: Authentication error | troubled for 30m"
        ]
    );
}

#[test]
fn a_notified_streak_that_ends_says_it_recovered_once() {
    let mut runs = every_sweep(Health::Failing, 30);
    runs.extend([(45, Health::Converged), (60, Health::Converged)]);
    assert_eq!(
        notices(&runs, always),
        [
            "30: norn failing on oracle-1 / unread: worker route example.com: cloudflare answered 403: Authentication error | troubled for 30m",
            "45: norn recovered on oracle-1 / converged after 45m troubled",
        ]
    );
}

#[test]
fn a_streak_that_ends_before_it_was_told_says_nothing() {
    let runs = [
        (0, Health::Failing),
        (15, Health::Failing),
        (30, Health::Converged),
    ];
    assert!(notices(&runs, always).is_empty());
}

#[test]
fn held_is_no_trouble_and_ends_a_streak() {
    assert!(notices(&every_sweep(Health::Held, 120), always).is_empty());
    let runs = [
        (0, Health::Failing),
        (15, Health::Held),
        (30, Health::Failing),
        (45, Health::Failing),
    ];
    assert!(notices(&runs, always).is_empty());
}

#[test]
fn a_held_run_after_a_notified_streak_is_a_recovery() {
    let mut runs = every_sweep(Health::Blocked, 30);
    runs.push((45, Health::Held));
    assert_eq!(
        notices(&runs, always),
        [
            "30: norn blocked on oracle-1 / blocked: dns record example.com grafana.example.com: an object norn does not own sits in this slot | troubled for 30m",
            "45: norn recovered on oracle-1 / held after 45m troubled",
        ]
    );
}

#[test]
fn a_change_of_trouble_within_a_streak_says_nothing_new() {
    let runs = [
        (0, Health::Blocked),
        (15, Health::Skipped),
        (30, Health::Skipped),
        (45, Health::Failing),
        (60, Health::Blocked),
    ];
    assert_eq!(
        notices(&runs, always),
        [
            "30: norn skipping runs on oracle-1 / no cloudflare token yet; skipping this run | troubled for 30m"
        ]
    );
}

#[test]
fn a_new_streak_after_a_recovery_is_told_again() {
    let mut runs = every_sweep(Health::Failing, 30);
    runs.push((45, Health::Converged));
    runs.extend(
        every_sweep(Health::Failing, 60)
            .into_iter()
            .map(|(minute, health)| (minute + 60, health)),
    );
    let sent = notices(&runs, always);
    assert_eq!(sent.len(), 3, "{sent:#?}");
    assert!(
        sent[2].starts_with("90: norn failing on oracle-1"),
        "{sent:#?}"
    );
}

#[test]
fn a_notice_no_device_took_is_sent_again_by_the_next_run() {
    let runs = every_sweep(Health::Failing, 60);
    let sent = notices(&runs, |minute| minute >= 45);
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert!(sent[0].starts_with("30: norn failing"), "{sent:#?}");
    assert!(sent[1].ends_with("troubled for 45m"), "{sent:#?}");

    // the devices still show the trouble, so a streak that starts before
    // the recovery gets through is the same one to them
    let mut runs = every_sweep(Health::Failing, 30);
    runs.extend([
        (45, Health::Converged),
        (60, Health::Failing),
        (75, Health::Failing),
        (90, Health::Failing),
        (105, Health::Converged),
    ]);
    assert_eq!(
        notices(&runs, |minute| minute != 45),
        [
            "30: norn failing on oracle-1 / unread: worker route example.com: cloudflare answered 403: Authentication error | troubled for 30m",
            "45: norn recovered on oracle-1 / converged after 45m troubled",
            "105: norn recovered on oracle-1 / converged after 45m troubled",
        ]
    );
}

#[test]
fn a_long_line_is_cut_and_the_rest_counted() {
    let now = start() + TimeDelta::minutes(150);
    let mut current = Status::following(None, &Summary::of(&[]), now);
    current.health = Health::Failing;
    current.troubled_since = Some(start());
    current.lines = vec!["x".repeat(400), "second".into(), "third".into()];
    let Some(Notice { turn, body, .. }) = notice(None, &current, AFTER, "oracle-1") else {
        panic!("no notice");
    };
    assert_eq!(turn, Turn::Trouble);
    let (first, last) = body.split_once('\n').unwrap();
    assert_eq!(first, format!("{}… (+2 more)", "x".repeat(160)));
    assert_eq!(last, "troubled for 2h30m");
}

fn settings(vapid: &Path, subscriptions: &Path, after: Duration) -> PushSettings {
    PushSettings {
        vapid_key_file: vapid.to_owned(),
        subscriptions_file: subscriptions.to_owned(),
        subject: "https://fleet.example.com".into(),
        navigate: "https://fleet.example.com/machines/{host}".into(),
        after,
    }
}

#[test]
fn a_notice_is_a_declarative_message_tagged_for_its_host() {
    let now = start() + TimeDelta::minutes(30);
    let mut current = Status::following(None, &summary(Health::Failing), start());
    current = Status::following(Some(&current), &summary(Health::Failing), now);
    let notice = notice(None, &current, AFTER, "oracle-1").unwrap();
    let message = message(
        &notice,
        &current,
        &settings(Path::new("unused"), Path::new("unused"), AFTER),
        "oracle-1",
    );
    let payload: Value = serde_json::from_slice(message.payload()).unwrap();
    assert_eq!(
        payload,
        json!({
            "web_push": 8030,
            "notification": {
                "title": "norn failing on oracle-1",
                "body": "unread: worker route example.com: cloudflare answered 403: Authentication error\ntroubled for 30m",
                "navigate": "https://fleet.example.com/machines/oracle-1",
                "tag": "norn/oracle-1",
                "timestamp": now.timestamp_millis(),
                "lang": "en",
                "dir": "ltr",
            }
        })
    );
}

/// what the fake push service was sent
struct Pushed {
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// a push service on 127.0.0.1 that answers each request with the next
/// status queued, or 201 once none is left
struct PushService {
    endpoint: String,
    received: Arc<Mutex<Vec<Pushed>>>,
    answers: Arc<Mutex<VecDeque<u16>>>,
}

impl PushService {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/push/phone", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let answers = Arc::new(Mutex::new(VecDeque::new()));
        let (log, queue) = (received.clone(), answers.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let mut headers = BTreeMap::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    let Some((name, value)) = line.trim_end().split_once(':') else {
                        break;
                    };
                    headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
                }
                let length = headers
                    .get("content-length")
                    .and_then(|length| length.parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                log.lock().unwrap().push(Pushed {
                    path,
                    headers,
                    body,
                });
                let status = queue.lock().unwrap().pop_front().unwrap_or(201);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Answer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        Self {
            endpoint,
            received,
            answers,
        }
    }

    fn count(&self) -> usize {
        self.received.lock().unwrap().len()
    }

    fn answer(&self, statuses: &[u16]) {
        self.answers.lock().unwrap().extend(statuses);
    }
}

/// a machine with push settings for one device behind `service`
struct Pushing {
    machine: Machine,
    receiver: Receiver,
    settings: PushSettings,
}

impl Pushing {
    fn new(service: &PushService, after: Duration) -> Self {
        let machine = Machine::new(WEB_1);
        let receiver = Receiver::generate().unwrap();
        let mut device =
            serde_json::to_value(receiver.subscription(&service.endpoint).unwrap()).unwrap();
        device["name"] = json!("phone");
        let dir = machine.dir.path();
        let (vapid, subscriptions) = (dir.join("vapid"), dir.join("subscriptions.json"));
        std::fs::write(
            &vapid,
            gjallar::VapidKey::generate().unwrap().to_base64url() + "\n",
        )
        .unwrap();
        std::fs::write(&subscriptions, json!([device]).to_string()).unwrap();
        let settings = settings(&vapid, &subscriptions, after);
        Self {
            machine,
            receiver,
            settings,
        }
    }

    fn converge(&self, fake: &Arc<super::fake::Cloudflare>) -> Health {
        let desired = self.machine.declare(ENFORCE, web_1().into());
        converge_with(
            &Cloudflare::Fake(fake.clone()),
            &desired,
            Some(&self.machine.token()),
            &self.machine.state(),
            timing(),
            MAX_DELETES,
            None,
            Some(&self.settings),
        )
    }

    fn status(&self) -> Status {
        status::read(&self.machine.state()).unwrap()
    }

    fn notification(&self, pushed: &Pushed) -> Value {
        let payload = self.receiver.decrypt(&pushed.body).unwrap();
        serde_json::from_slice::<Value>(&payload).unwrap()["notification"].clone()
    }
}

#[test]
fn a_revoked_token_is_told_once_after_a_while_and_its_restoring_once() {
    let fake = cloudflare();
    let service = PushService::start();
    let one = Pushing::new(&service, Duration::from_millis(300));

    assert_eq!(one.converge(&fake), Health::Converged);
    fake.revoke(true);
    assert_eq!(one.converge(&fake), Health::Failing);
    assert_eq!(service.count(), 0);

    thread::sleep(Duration::from_millis(400));
    assert_eq!(one.converge(&fake), Health::Failing);
    assert_eq!(service.count(), 1);
    assert!(one.status().notified_at.is_some());
    assert_eq!(one.converge(&fake), Health::Failing);
    assert_eq!(service.count(), 1);

    fake.revoke(false);
    assert_eq!(one.converge(&fake), Health::Converged);
    assert_eq!(service.count(), 2);
    assert_eq!(one.status().notified_at, None);
    assert_eq!(one.converge(&fake), Health::Converged);
    assert_eq!(service.count(), 2);

    let received = service.received.lock().unwrap();
    let (trouble, recovery) = (&received[0], &received[1]);
    assert_eq!(trouble.path, "/push/phone");
    assert_eq!(trouble.headers["urgency"], "high");
    assert_eq!(trouble.headers["ttl"], "86400");
    assert_eq!(trouble.headers["content-encoding"], "aes128gcm");
    assert_eq!(trouble.headers["topic"], recovery.headers["topic"]);
    assert!(trouble.headers["authorization"].starts_with("vapid t="));
    assert_eq!(recovery.headers["urgency"], "normal");

    let told = one.notification(trouble);
    assert_eq!(told["title"], "norn failing on web-1");
    assert_eq!(told["tag"], "norn/web-1");
    assert_eq!(told["navigate"], "https://fleet.example.com/machines/web-1");
    let body = told["body"].as_str().unwrap();
    assert!(
        body.starts_with("unread: dns record example.com: ") && body.contains("Invalid API Token"),
        "{body}"
    );
    let recovered = one.notification(recovery);
    assert_eq!(recovered["title"], "norn recovered on web-1");
    assert_eq!(recovered["tag"], "norn/web-1");
    assert!(
        recovered["body"]
            .as_str()
            .unwrap()
            .starts_with("converged after "),
        "{recovered}"
    );
}

#[test]
fn a_busy_push_service_leaves_the_notice_to_the_next_run() {
    let fake = cloudflare();
    let service = PushService::start();
    let one = Pushing::new(&service, Duration::ZERO);
    fake.revoke(true);

    // 503 twice: the first try and its one retry
    service.answer(&[503, 503]);
    assert_eq!(one.converge(&fake), Health::Failing);
    assert_eq!(service.count(), 2);
    assert_eq!(one.status().notified_at, None);

    assert_eq!(one.converge(&fake), Health::Failing);
    assert_eq!(service.count(), 3);
    assert!(one.status().notified_at.is_some());
}

#[test]
fn a_gone_subscription_counts_as_told() {
    let fake = cloudflare();
    let service = PushService::start();
    let one = Pushing::new(&service, Duration::ZERO);
    fake.revoke(true);
    service.answer(&[410]);
    assert_eq!(one.converge(&fake), Health::Failing);
    assert!(one.status().notified_at.is_some());
    assert_eq!(one.converge(&fake), Health::Failing);
    assert_eq!(service.count(), 1);
}

#[test]
fn unreadable_push_files_change_nothing_about_the_run() {
    let fake = cloudflare();
    let service = PushService::start();
    let one = Pushing::new(&service, Duration::ZERO);
    std::fs::remove_file(&one.settings.subscriptions_file).unwrap();
    fake.revoke(true);
    assert_eq!(one.converge(&fake), Health::Failing);
    assert_eq!(service.count(), 0);
    assert_eq!(one.status().notified_at, None);

    std::fs::write(&one.settings.subscriptions_file, "[]").unwrap();
    assert_eq!(one.converge(&fake), Health::Failing);
    assert!(one.status().notified_at.is_some());
}
