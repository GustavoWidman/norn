//! cloudflare workers, as two kinds of fleet object: a worker's script, in
//! the account, and the routes that run it, in each zone. both are shared by
//! the whole fleet; no one machine owns a worker.

mod mark;
mod value;
mod wire;

pub(crate) use mark::fits;
pub(crate) use value::{Account, CompatibilityDate, Module, Pattern, Route, Script, ScriptName};

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::{
    cloudflare::{Api, ApiError, Capability, Verb, Write},
    dns::ZoneId,
    kind::{Born, Kind, Live, Mark, Ownership, Reading, Sent, Stamp, Step, Want},
    model::{Owner, Revision, Zone},
};

#[derive(Clone, Debug)]
pub(crate) struct Scripts;

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct ScriptHandle {
    name: ScriptName,
    /// the tags that are not norn's, which every write keeps
    others: Vec<String>,
}

impl Kind for Scripts {
    const NAME: &'static str = "worker script";
    const ADOPTS: bool = false;

    type Context = ();
    type Scope = Account;
    type Place = Account;
    type Key = ScriptName;
    type Name = ScriptName;
    type Identity = ScriptName;
    type Value = Script;
    type Handle = ScriptHandle;

    fn key(script: &Script) -> ScriptName {
        script.name.clone()
    }

    fn name(name: &ScriptName) -> ScriptName {
        name.clone()
    }

    fn identity(script: &Script) -> ScriptName {
        script.name.clone()
    }

    fn excludes(_: &ScriptName, _: &ScriptName) -> bool {
        false
    }

    fn read<C: Capability>(api: &Api<C>, account: &Account, _: &()) -> Reading<Scripts> {
        let scripts: Vec<wire::LiveScript> =
            api.get(&format!("/accounts/{account}/workers/scripts"), &[])?;
        let live = scripts
            .into_iter()
            .filter_map(|script| {
                let Ok(name) = ScriptName::parse(&script.id) else {
                    log::warn!(
                        "{account}: skipping worker {:?}, a name norn cannot spell",
                        script.id
                    );
                    return None;
                };
                let read = mark::read(&name, script.tags.as_deref().unwrap_or_default());
                Some(Live {
                    handle: ScriptHandle {
                        name: name.clone(),
                        others: read.others,
                    },
                    key: name,
                    value: read.value,
                    ownership: read.ownership,
                    born: Born {
                        created: script.created_on.unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
                        id: script.id,
                    },
                })
            })
            .collect();
        Ok((account.clone(), live))
    }

    fn write(
        api: &Api<Write>,
        account: &Account,
        step: &Step<Scripts>,
        mark: &Mark,
    ) -> Result<Sent, ApiError> {
        let Mark::Fleet(revision) = mark else {
            unreachable!("a worker is the fleet's; no machine marks one")
        };
        let at = |name: &ScriptName| format!("/accounts/{account}/workers/scripts/{name}");
        let tags = |live: &Live<Scripts>, script: &Script, retiring| wire::Settings {
            tags: mark::write(*revision, retiring, script, &live.handle.others),
        };
        let written = |live: &Live<Scripts>| {
            live.value
                .clone()
                .expect("a fleet script read back what it was written with")
        };
        let sent: Result<serde_json::Value, ApiError> = match step {
            Step::Create { want, .. } => api.upload(
                Verb::Put,
                &at(&want.name),
                wire::upload(want, mark::write(*revision, false, want, &[])),
            ),
            Step::Update { live, want, .. } => {
                let code_unchanged = live.value.as_ref().is_some_and(|live| {
                    live.module == want.module && live.compatibility_date == want.compatibility_date
                });
                if code_unchanged {
                    api.send(
                        Verb::Patch,
                        &format!("{}/script-settings", at(&want.name)),
                        Some(&tags(live, want, false)),
                    )
                } else {
                    let tags = mark::write(*revision, false, want, &live.handle.others);
                    api.upload(Verb::Put, &at(&want.name), wire::upload(want, tags))
                }
            }
            Step::Retire { live, .. } | Step::Restore { live, .. } => {
                let retiring = matches!(step, Step::Retire { .. });
                api.send(
                    Verb::Patch,
                    &format!("{}/script-settings", at(&live.key)),
                    Some(&tags(live, &written(live), retiring)),
                )
            }
            Step::Delete { live, .. } => api.send(Verb::Delete, &at(&live.key), None::<&()>),
            Step::Adopt { .. } => unreachable!("a worker script is never adopted"),
        };
        match (sent, step) {
            (Ok(_), _) | (Err(ApiError::NotFound), Step::Delete { .. }) => Ok(Sent::Applied),
            (Err(error), _) => Err(error),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Routes;

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct RouteId(String);

/// what tells a route's owner, since a route has no field of its own to
/// carry a mark: the fleet's scripts, whose tags list the patterns of the
/// fleet's routes, and the patterns this configuration declares
#[derive(Debug, Default)]
pub(crate) struct Owners {
    scripts: BTreeMap<ScriptName, (Stamp, Script)>,
    declared: BTreeSet<Pattern>,
    own: Option<Revision>,
}

impl Owners {
    pub fn new<'a>(
        scripts: impl Iterator<Item = &'a Live<Scripts>>,
        routes: &[Want<Routes>],
        own: Option<Revision>,
    ) -> Self {
        Self {
            scripts: scripts
                .filter_map(|live| match (&live.ownership, &live.value) {
                    (Ownership::Fleet(stamp), Some(script)) => {
                        Some((live.key.clone(), (*stamp, script.clone())))
                    }
                    _ => None,
                })
                .collect(),
            declared: routes
                .iter()
                .map(|want| want.value.pattern.clone())
                .collect(),
            own,
        }
    }

    /// a route is the fleet's when a fleet script lists its pattern: as a
    /// route, for one that runs that script, or as a bypass, for one that
    /// runs no worker. it is stamped as that script is. one this
    /// configuration no longer declares counts as retiring at this
    /// configuration's revision, so it is deleted once the whole fleet runs
    /// a configuration without it. any other route is someone else's, even
    /// one that runs a fleet script.
    fn of(&self, route: &Route) -> Ownership {
        let lists = |(_, script): &&(Stamp, Script)| match &route.script {
            Some(_) => script.routes.contains(&route.pattern),
            None => script.bypass.contains(&route.pattern),
        };
        let listing = match &route.script {
            Some(name) => self.scripts.get(name).filter(lists),
            None => self.scripts.values().find(lists),
        };
        match listing {
            Some((stamp, _)) if self.declared.contains(&route.pattern) => Ownership::Fleet(Stamp {
                revision: stamp.revision,
                retiring: false,
            }),
            Some((stamp, _)) => Ownership::Fleet(Stamp {
                revision: self.own.unwrap_or(stamp.revision),
                retiring: true,
            }),
            None => Ownership::Unmarked { annotated: false },
        }
    }
}

/// what each declared script is to list: its declared patterns where no
/// route someone else made stands, and the patterns it lists now whose
/// routes are still there, so a dropped route stays the fleet's until it is
/// deleted
pub(crate) fn listings<'a>(
    wants: &mut [Want<Scripts>],
    scripts: impl Iterator<Item = &'a Live<Scripts>>,
    routes: impl Iterator<Item = &'a Live<Routes>>,
) {
    let routes: BTreeMap<&Pattern, &Live<Routes>> = routes.map(|live| (&live.key, live)).collect();
    let listed: BTreeMap<&ScriptName, &Script> = scripts
        .filter(|live| matches!(live.ownership, Ownership::Fleet(_)))
        .filter_map(|live| Some((&live.key, live.value.as_ref()?)))
        .collect();
    for want in wants {
        let script = &mut want.value;
        let free = |pattern: &Pattern| {
            routes
                .get(pattern)
                .is_none_or(|live| matches!(live.ownership, Ownership::Fleet(_)))
        };
        script.routes.retain(free);
        script.bypass.retain(free);
        let Some(listed) = listed.get(&script.name) else {
            continue;
        };
        let runs = |pattern: &&Pattern, script: Option<&ScriptName>| {
            routes
                .get(*pattern)
                .and_then(|live| live.value.as_ref())
                .is_some_and(|route| route.script.as_ref() == script)
        };
        let name = script.name.clone();
        script.routes.extend(
            listed
                .routes
                .iter()
                .filter(|p| runs(p, Some(&name)))
                .cloned(),
        );
        script
            .bypass
            .extend(listed.bypass.iter().filter(|p| runs(p, None)).cloned());
    }
}

impl Kind for Routes {
    const NAME: &'static str = "worker route";
    const ADOPTS: bool = false;

    type Context = Owners;
    type Scope = Zone;
    type Place = ZoneId;
    type Key = Pattern;
    type Name = Pattern;
    type Identity = Pattern;
    type Value = Route;
    type Handle = RouteId;

    fn key(route: &Route) -> Pattern {
        route.pattern.clone()
    }

    fn name(pattern: &Pattern) -> Pattern {
        pattern.clone()
    }

    fn identity(route: &Route) -> Pattern {
        route.pattern.clone()
    }

    fn excludes(_: &Pattern, _: &Pattern) -> bool {
        false
    }

    fn read<C: Capability>(api: &Api<C>, zone: &Zone, owners: &Owners) -> Reading<Routes> {
        let id = ZoneId::find(api, zone)?;
        let routes: Vec<wire::LiveRoute> =
            api.get(&format!("/zones/{}/workers/routes", id.0), &[])?;
        let live = routes
            .into_iter()
            .map(|route| {
                let pattern = Pattern::live(&route.pattern);
                let script = route.script.filter(|script| !script.is_empty());
                let value = match script.as_deref().map(ScriptName::parse) {
                    None => Some(Route {
                        pattern: pattern.clone(),
                        script: None,
                    }),
                    Some(Ok(script)) => Some(Route {
                        pattern: pattern.clone(),
                        script: Some(script),
                    }),
                    Some(Err(_)) => None,
                };
                Live {
                    handle: RouteId(route.id.clone()),
                    ownership: value
                        .as_ref()
                        .map(|value| owners.of(value))
                        .unwrap_or(Ownership::Unmarked { annotated: false }),
                    key: pattern,
                    value,
                    born: Born {
                        created: DateTime::<Utc>::UNIX_EPOCH,
                        id: route.id,
                    },
                }
            })
            .collect();
        Ok((id, live))
    }

    /// a route carries no mark, so `mark` only says the gate let this run
    /// write
    fn write(
        api: &Api<Write>,
        zone: &ZoneId,
        step: &Step<Routes>,
        _: &Mark,
    ) -> Result<Sent, ApiError> {
        let routes = format!("/zones/{}/workers/routes", zone.0);
        let at = |live: &Live<Routes>| format!("{routes}/{}", live.handle.0);
        fn body(route: &Route) -> wire::RouteBody<'_> {
            wire::RouteBody {
                pattern: route.pattern.as_str(),
                script: route.script.as_ref().map(ScriptName::as_str),
            }
        }
        let sent: Result<serde_json::Value, ApiError> = match step {
            Step::Create { want, .. } => api.send(Verb::Post, &routes, Some(&body(want))),
            Step::Update { live, want, .. } => api.send(Verb::Put, &at(live), Some(&body(want))),
            Step::Delete { live, .. } => api.send(Verb::Delete, &at(live), None::<&()>),
            Step::Adopt { .. } | Step::Retire { .. } | Step::Restore { .. } => {
                unreachable!("a route carries no mark to adopt, retire or restore")
            }
        };
        match (sent, step) {
            (Ok(_), _) | (Err(ApiError::NotFound), Step::Delete { .. }) => Ok(Sent::Applied),
            // cloudflare refuses a second route with one pattern (10020).
            // whether another machine of the fleet made it, or a person did,
            // only the route there now says
            (
                Err(error @ ApiError::Rejected { .. }),
                Step::Create {
                    owner: Owner::Fleet,
                    want,
                },
            ) => {
                let now: Result<Vec<wire::LiveRoute>, ApiError> = api.get(&routes, &[]);
                let raced = now.is_ok_and(|now| {
                    now.into_iter().any(|route| {
                        route.pattern == want.pattern.as_str()
                            && route.script.filter(|script| !script.is_empty()).as_deref()
                                == want.script.as_ref().map(ScriptName::as_str)
                    })
                });
                if raced { Ok(Sent::Raced) } else { Err(error) }
            }
            (Err(error), _) => Err(error),
        }
    }
}
