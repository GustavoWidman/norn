//! Web Push notices to the owner's devices: one when runs have stayed
//! troubled for a while, and one when that ends. nothing else, so a fault
//! that lasts for days is still one notice

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use eyre::{WrapErr, bail, ensure, eyre};
use gjallar::{
    Client, Dir, Message, Notification, Outcome, Subscription, Topic, Urgency, VapidKey, WebPush,
};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::status::{Health, Status};

/// how long one push service may take to answer
const TIMEOUT: Duration = Duration::from_secs(5);
/// how long a whole send may take, retries included
pub(crate) const DEADLINE: Duration = Duration::from_secs(10);
/// a notice older than a day is noise
const TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// the most of a status line a notice quotes
const QUOTE: usize = 160;

/// where notices go and when. both files are read at every send, so a
/// deploy that changes them takes effect on the next run
#[derive(Clone, Debug)]
pub struct PushSettings {
    /// the base64url of a P-256 private scalar
    pub vapid_key_file: PathBuf,
    /// a JSON array of `PushSubscription.toJSON()` objects, each with a `name`
    pub subscriptions_file: PathBuf,
    /// the VAPID subject: an `https:` URL or a `mailto:`
    pub subject: String,
    /// the absolute URL a tap opens, with `{host}` standing for this machine
    pub navigate: String,
    /// how long runs stay troubled before the devices hear of it
    pub after: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Turn {
    Trouble,
    Recovery,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub turn: Turn,
    pub title: String,
    pub body: String,
}

/// what, if anything, the devices should hear about the run that wrote
/// `current`: trouble once a streak is `after` old and they have not heard
/// of it, a recovery once a streak they heard of ends
pub(crate) fn notice(
    previous: Option<&Status>,
    current: &Status,
    after: Duration,
    host: &str,
) -> Option<Notice> {
    match (current.troubled_since, current.notified_at) {
        (Some(since), None)
            if (current.finished - since)
                .to_std()
                .is_ok_and(|age| age >= after) =>
        {
            let mut body = current
                .lines
                .first()
                .map_or_else(String::new, |line| quote(line));
            if current.lines.len() > 1 {
                body.push_str(&format!(" (+{} more)", current.lines.len() - 1));
            }
            body.push_str(&format!(
                "\ntroubled for {}",
                span(current.finished - since)
            ));
            let title = match current.health {
                Health::Skipped => format!("norn skipping runs on {host}"),
                health => format!("norn {} on {host}", health.as_str()),
            };
            Some(Notice {
                turn: Turn::Trouble,
                title,
                body: body.trim_start().to_owned(),
            })
        }
        (None, Some(_)) => {
            let lasted = previous
                .and_then(|previous| previous.troubled_since)
                .map(|since| format!(" after {} troubled", span(current.finished - since)));
            Some(Notice {
                turn: Turn::Recovery,
                title: format!("norn recovered on {host}"),
                body: format!("{}{}", current.health.as_str(), lasted.unwrap_or_default()),
            })
        }
        _ => None,
    }
}

/// records in `status` that the devices heard `notice`
pub(crate) fn settle(status: &mut Status, notice: &Notice) {
    status.notified_at = match notice.turn {
        Turn::Trouble => Some(status.finished),
        Turn::Recovery => None,
    };
}

/// sends the notice the run that wrote `current` calls for, if any, and
/// records that the devices heard it. a notice that reached no device
/// while one may still take it is left for the next run to send again.
/// nothing here fails the run
pub(crate) fn announce(
    state_dir: &Path,
    previous: Option<&Status>,
    mut current: Status,
    settings: &PushSettings,
    host: &str,
) {
    let Some(notice) = notice(previous, &current, settings.after, host) else {
        return;
    };
    let sent = match Sender::load(settings) {
        Ok(sender) => sender.send(&message(&notice, &current, settings, host)),
        Err(error) => {
            log::warn!(
                "cannot send the push notice \"{}\": {error:#}",
                notice.title
            );
            return;
        }
    };
    if heard(&sent) {
        settle(&mut current, &notice);
        crate::status::save(state_dir, &current);
    } else {
        log::warn!(
            "no device took the push notice \"{}\"; the next run sends it again",
            notice.title
        );
    }
}

fn heard(sent: &[(String, Result<Outcome, gjallar::Error>)]) -> bool {
    sent.is_empty()
        || sent.iter().any(|(_, result)| match result {
            Ok(Outcome::RetryLater { .. }) | Err(_) => false,
            Ok(_) => true,
        })
}

pub(crate) fn message(
    notice: &Notice,
    current: &Status,
    settings: &PushSettings,
    host: &str,
) -> Message {
    let tag = format!("norn/{host}");
    let notification = Notification::new(&notice.title, settings.navigate.replace("{host}", host))
        .body(&notice.body)
        .tag(&tag)
        .timestamp(SystemTime::from(current.finished))
        .lang("en")
        .dir(Dir::Ltr);
    let urgency = match notice.turn {
        Turn::Trouble => Urgency::High,
        Turn::Recovery => Urgency::Normal,
    };
    Message::declarative(notification)
        .expect("a notice quotes at most a line, far under a push message's size")
        .ttl(TTL)
        .urgency(urgency)
        .topic(Topic::digest(&tag))
}

struct Sender {
    client: Client,
    devices: Vec<Device>,
}

/// one entry of the subscription list. the endpoint and its keys are
/// secrets, so only the name is ever logged
struct Device {
    name: String,
    subscription: Subscription,
}

impl Sender {
    fn load(settings: &PushSettings) -> eyre::Result<Self> {
        ensure!(
            settings.navigate.starts_with("https://"),
            "the push navigate URL {:?} is not an https URL",
            settings.navigate
        );
        let key = read(&settings.vapid_key_file)?;
        let key = std::str::from_utf8(&key)
            .ok()
            .and_then(|text| VapidKey::from_base64url(text.trim()).ok())
            .ok_or_else(|| {
                eyre!(
                    "{} does not hold a VAPID private key (the base64url of a P-256 scalar)",
                    settings.vapid_key_file.display()
                )
            })?;
        let devices = devices(&read(&settings.subscriptions_file)?)
            .wrap_err_with(|| format!("in {}", settings.subscriptions_file.display()))?;
        let push = WebPush::new(key, &settings.subject)?;
        Ok(Self {
            client: Client::new(push).timeout(TIMEOUT).deadline(DEADLINE),
            devices,
        })
    }

    fn send(&self, message: &Message) -> Vec<(String, Result<Outcome, gjallar::Error>)> {
        let subscriptions = self.devices.iter().map(|device| &device.subscription);
        let sent: Vec<_> = self
            .devices
            .iter()
            .map(|device| device.name.clone())
            .zip(self.client.send_all(subscriptions, message))
            .collect();
        for (name, result) in &sent {
            match result {
                Ok(Outcome::Delivered) => log::info!("push notice sent to {name}"),
                Ok(Outcome::Gone { status }) => log::warn!(
                    "push subscription {name:?} is gone ({}); remove it from the subscription list",
                    status.as_u16()
                ),
                Ok(Outcome::Refused { status, reason }) => log::warn!(
                    "push service refused the notice for {name:?} ({}): {}",
                    status.as_u16(),
                    reason.as_deref().unwrap_or("no reason given")
                ),
                Ok(other) => log::warn!("push notice for {name:?} not taken: {other:?}"),
                Err(error) => log::warn!("push notice for {name:?} not sent: {error}"),
            }
        }
        sent
    }
}

fn read(path: &Path) -> eyre::Result<Zeroizing<Vec<u8>>> {
    std::fs::read(path)
        .map(Zeroizing::new)
        .wrap_err_with(|| format!("cannot read {}", path.display()))
}

fn devices(json: &[u8]) -> eyre::Result<Vec<Device>> {
    #[derive(Deserialize)]
    struct Entry {
        name: String,
        #[serde(flatten)]
        subscription: serde_json::Value,
    }

    let entries: Vec<Entry> = serde_json::from_slice(json)
        .wrap_err("expected a JSON array of subscriptions, each with a name")?;
    let mut names = BTreeSet::new();
    entries
        .into_iter()
        .map(|Entry { name, subscription }| {
            ensure!(!name.trim().is_empty(), "a subscription has an empty name");
            if !names.insert(name.clone()) {
                bail!("two subscriptions are named {name:?}");
            }
            let subscription = serde_json::from_value(subscription)
                .wrap_err_with(|| format!("subscription {name:?}"))?;
            Ok(Device { name, subscription })
        })
        .collect()
}

fn quote(line: &str) -> String {
    match line.char_indices().nth(QUOTE) {
        Some((cut, _)) => format!("{}…", line[..cut].trim_end()),
        None => line.to_owned(),
    }
}

/// a duration as a person says it: `45s`, `30m`, `2h05m`
fn span(duration: chrono::TimeDelta) -> String {
    let seconds = duration.num_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60)
    }
}
