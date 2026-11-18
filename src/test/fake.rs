use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};

use crate::cloudflare::{Answer, Deliver, Fetch, Payload, Verb};

type Interleaved = Box<dyn FnOnce() + Send>;

pub(crate) struct Cloudflare {
    state: Mutex<State>,
    page_cap: usize,
    interleaved: Mutex<Option<Interleaved>>,
}

#[derive(Default)]
struct State {
    zones: BTreeMap<String, String>,
    records: BTreeMap<String, Vec<Value>>,
    /// by account, then by name
    scripts: BTreeMap<String, BTreeMap<String, Script>>,
    /// by zone id
    routes: BTreeMap<String, Vec<Value>>,
    /// zone ids whose routes the token may not touch
    forbidden: BTreeSet<String>,
    requests: Vec<String>,
    serial: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Script {
    pub listed: Value,
    pub module: Vec<u8>,
}

impl Cloudflare {
    pub fn new(page_cap: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            page_cap,
            interleaved: Mutex::default(),
        })
    }

    /// runs `other` right before the next write is handled, as another
    /// machine whose run lands between this one's read and its write
    pub fn before_next_write(&self, other: impl FnOnce() + Send + 'static) {
        *self.interleaved.lock().unwrap() = Some(Box::new(other));
    }

    pub fn seed(&self, zone: &str, mut record: Value) {
        let mut state = self.state.lock().unwrap();
        let id = state.zone_id(zone);
        if record.get("id").is_none() {
            state.stamp(&mut record);
        }
        state.records.entry(id).or_default().push(record);
    }

    pub fn account(&self, account: &str) {
        let mut state = self.state.lock().unwrap();
        state.scripts.entry(account.to_owned()).or_default();
    }

    /// a script uploaded some other way, carrying `tags`
    pub fn seed_script(&self, account: &str, name: &str, tags: &[&str]) {
        let mut state = self.state.lock().unwrap();
        let listed = json!({
            "id": name,
            "tags": tags,
            "compatibility_date": "2025-01-01",
            "created_on": "2026-01-01T00:00:00Z",
            "modified_on": "2026-01-01T00:00:00Z",
        });
        state.scripts.entry(account.to_owned()).or_default().insert(
            name.to_owned(),
            Script {
                listed,
                module: b"export default {}".to_vec(),
            },
        );
    }

    /// replaces a script's tags, as a person editing them would
    pub fn retag(&self, account: &str, name: &str, tags: &[String]) {
        let mut state = self.state.lock().unwrap();
        state
            .scripts
            .get_mut(account)
            .unwrap()
            .get_mut(name)
            .unwrap()
            .listed["tags"] = json!(tags);
    }

    pub fn seed_route(&self, zone: &str, pattern: &str, script: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        let id = state.zone_id(zone);
        state.serial += 1;
        let route =
            json!({ "id": format!("{:032x}", state.serial), "pattern": pattern, "script": script });
        state.routes.entry(id).or_default().push(route);
    }

    /// a token without workers routes permission on `zone`: its routes
    /// answer 403
    pub fn forbid_routes(&self, zone: &str) {
        let mut state = self.state.lock().unwrap();
        let id = state.zone_id(zone);
        state.forbidden.insert(id);
    }

    pub fn scripts(&self, account: &str) -> BTreeMap<String, Script> {
        let state = self.state.lock().unwrap();
        state.scripts.get(account).cloned().unwrap_or_default()
    }

    /// the zone's routes, as `(pattern, script)`, by pattern
    pub fn routes(&self, zone: &str) -> Vec<(String, Option<String>)> {
        let state = self.state.lock().unwrap();
        let id = state
            .zones
            .iter()
            .find(|(_, name)| *name == zone)
            .map(|(id, _)| id.clone());
        let mut routes: Vec<_> = id
            .and_then(|id| state.routes.get(&id).cloned())
            .unwrap_or_default()
            .into_iter()
            .map(|route| {
                (
                    route["pattern"].as_str().unwrap().to_owned(),
                    route["script"].as_str().map(str::to_owned),
                )
            })
            .collect();
        routes.sort();
        routes
    }

    pub fn writes(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .requests
            .iter()
            .filter(|request| !request.starts_with("GET "))
            .cloned()
            .collect()
    }

    pub fn requests(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    pub fn records(&self, zone: &str) -> Vec<Value> {
        let state = self.state.lock().unwrap();
        let id = state
            .zones
            .iter()
            .find(|(_, name)| *name == zone)
            .map(|(id, _)| id.clone());
        id.and_then(|id| state.records.get(&id).cloned())
            .unwrap_or_default()
    }
}

impl State {
    fn zone_id(&mut self, zone: &str) -> String {
        if let Some((id, _)) = self.zones.iter().find(|(_, name)| *name == zone) {
            return id.clone();
        }
        let id = format!("{:032x}", 0xa000 + self.zones.len());
        self.zones.insert(id.clone(), zone.to_owned());
        id
    }

    fn stamp(&mut self, record: &mut Value) {
        self.serial += 1;
        record["id"] = json!(format!("{:032x}", self.serial));
        record["created_on"] = json!(format!(
            "2026-01-01T00:00:{:02}.{:06}Z",
            self.serial % 60,
            self.serial
        ));
        record["modified_on"] = record["created_on"].clone();
        settle(record);
    }
}

fn settle(record: &mut Value) {
    let proxiable = matches!(record["type"].as_str(), Some("A" | "AAAA" | "CNAME"));
    if !proxiable {
        record["proxied"] = json!(false);
    }
    if record["proxied"] == json!(true) {
        record["ttl"] = json!(1);
    }
    if record.get("comment").is_none() {
        record["comment"] = Value::Null;
    }
}

fn ok(result: Value, info: Option<Value>) -> Answer {
    let mut body = json!({ "success": true, "errors": [], "messages": [], "result": result });
    if let Some(info) = info {
        body["result_info"] = info;
    }
    Ok((200, serde_json::to_vec(&body).unwrap()))
}

/// the 403 cloudflare gives a token that lacks workers routes permission on
/// a zone: pretty-printed, with an error that carries no code
const FORBIDDEN: &str = r#"{
  "result": null,
  "success": false,
  "errors": [
    {
      "message": "Authentication error"
    }
  ],
  "messages": []
}"#;

fn refused(status: u16, code: u32, message: &str) -> Answer {
    let body = json!({ "success": false, "errors": [{ "code": code, "message": message }], "messages": [], "result": null });
    Ok((status, serde_json::to_vec(&body).unwrap()))
}

impl Fetch for Cloudflare {
    fn get(&self, path: &str, query: &[(&str, &str)], _token: &str, _timeout: Duration) -> Answer {
        let mut state = self.state.lock().unwrap();
        let shown: Vec<String> = query
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        state
            .requests
            .push(format!("GET {path}?{}", shown.join("&")));
        let arg = |key: &str| {
            query
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, value)| *value)
        };

        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match parts.as_slice() {
            ["accounts", account, "workers", "scripts"] => {
                return match state.scripts.get(*account) {
                    Some(scripts) => {
                        let listed: Vec<Value> = scripts
                            .values()
                            .map(|script| script.listed.clone())
                            .collect();
                        ok(json!(listed), None)
                    }
                    None => refused(403, 10000, "Authentication error"),
                };
            }
            ["zones", zone, "workers", "routes"] => {
                if !state.records.contains_key(*zone) {
                    return refused(404, 7003, "Could not route to /zones");
                }
                if state.forbidden.contains(*zone) {
                    return Ok((403, FORBIDDEN.as_bytes().to_vec()));
                }
                let routes = state.routes.get(*zone).cloned().unwrap_or_default();
                return ok(json!(routes), None);
            }
            _ => {}
        }
        if path == "/zones" {
            let zones: Vec<Value> = state
                .zones
                .iter()
                .filter(|(_, name)| arg("name").is_none_or(|wanted| wanted == name.as_str()))
                .map(|(id, name)| json!({ "id": id, "name": name }))
                .collect();
            return ok(json!(zones), None);
        }
        let Some(zone) = path
            .strip_prefix("/zones/")
            .and_then(|rest| rest.strip_suffix("/dns_records"))
        else {
            return Ok((
                404,
                b"{\"success\":false,\"errors\":[],\"result\":null}".to_vec(),
            ));
        };
        let Some(records) = state.records.get(zone) else {
            return refused(404, 7003, "Could not route to /zones");
        };
        let page: usize = arg("page").and_then(|page| page.parse().ok()).unwrap_or(1);
        let per_page: usize = arg("per_page")
            .and_then(|n| n.parse().ok())
            .unwrap_or(100)
            .min(self.page_cap);
        let total_pages = records.len().div_ceil(per_page).max(1);
        let listed: Vec<Value> = records
            .iter()
            .skip((page - 1) * per_page)
            .take(per_page)
            .cloned()
            .collect();
        let info = json!({
            "page": page,
            "per_page": per_page,
            "count": listed.len(),
            "total_count": records.len(),
            "total_pages": total_pages,
        });
        ok(json!(listed), Some(info))
    }
}

impl Deliver for Cloudflare {
    fn send(
        &self,
        verb: Verb,
        path: &str,
        body: Option<Payload>,
        _token: &str,
        _timeout: Duration,
    ) -> Answer {
        let interleaved = self.interleaved.lock().unwrap().take();
        if let Some(other) = interleaved {
            other();
        }
        let mut state = self.state.lock().unwrap();
        let name = match verb {
            Verb::Post => "POST",
            Verb::Put => "PUT",
            Verb::Patch => "PATCH",
            Verb::Delete => "DELETE",
        };
        let (body, module) = match body {
            Some(payload) if payload.content_type.starts_with("multipart/form-data") => {
                let (metadata, module) = multipart(&payload);
                (Some(metadata), Some(module))
            }
            Some(payload) => (Some(serde_json::from_slice(&payload.bytes).unwrap()), None),
            None => (None, None),
        };
        let shown = body.as_ref().map(Value::to_string).unwrap_or_default();
        state
            .requests
            .push(format!("{name} {path} {shown}").trim_end().to_owned());

        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match parts.as_slice() {
            ["accounts", account, "workers", "scripts", script, rest @ ..] => {
                return state.script(verb, account, script, rest, body, module);
            }
            ["zones", zone, "workers", "routes", rest @ ..] => {
                return state.route(verb, zone, rest.first().copied(), body);
            }
            _ => {}
        }
        let (zone, id) = match parts.as_slice() {
            ["zones", zone, "dns_records"] => (zone.to_string(), None),
            ["zones", zone, "dns_records", id] => (zone.to_string(), Some(id.to_string())),
            _ => return refused(404, 7003, "Could not route"),
        };
        if !state.records.contains_key(&zone) {
            return refused(404, 7003, "Could not route to /zones");
        }

        match (verb, id) {
            (Verb::Post, None) => {
                let mut record = body.expect("a create has a body");
                let records = &state.records[&zone];
                let identical = records.iter().any(|existing| {
                    ["type", "name", "content"]
                        .iter()
                        .all(|field| existing[field] == record[field])
                });
                if identical {
                    return refused(400, 81058, "An identical record already exists.");
                }
                let addressed = |existing: &&Value| {
                    existing["name"] == record["name"]
                        && matches!(existing["type"].as_str(), Some("A" | "AAAA" | "CNAME"))
                };
                if record["type"] == "CNAME" && records.iter().any(|e| addressed(&e)) {
                    return refused(
                        400,
                        81053,
                        "An A, AAAA, or CNAME record with that host already exists.",
                    );
                }
                state.stamp(&mut record);
                state.records.get_mut(&zone).unwrap().push(record.clone());
                ok(record, None)
            }
            (verb, Some(id)) => {
                let records = state.records.get_mut(&zone).unwrap();
                let Some(index) = records.iter().position(|record| record["id"] == json!(id))
                else {
                    return refused(404, 81044, "Record does not exist.");
                };
                match verb {
                    Verb::Delete => {
                        records.remove(index);
                        ok(json!({ "id": id }), None)
                    }
                    Verb::Put | Verb::Patch => {
                        let record = &mut records[index];
                        if verb == Verb::Put {
                            let mut replaced = json!({ "settings": {}, "tags": [] });
                            for field in ["id", "created_on", "modified_on"] {
                                replaced[field] = record[field].clone();
                            }
                            *record = replaced;
                        }
                        for (field, value) in
                            body.expect("a rewrite has a body").as_object().unwrap()
                        {
                            record[field] = value.clone();
                        }
                        settle(record);
                        ok(record.clone(), None)
                    }
                    Verb::Post => refused(405, 10000, "Method not allowed"),
                }
            }
            (_, None) => refused(405, 10000, "Method not allowed"),
        }
    }
}

impl State {
    fn script(
        &mut self,
        verb: Verb,
        account: &str,
        name: &str,
        rest: &[&str],
        body: Option<Value>,
        module: Option<Vec<u8>>,
    ) -> Answer {
        self.serial += 1;
        let now = format!(
            "2026-01-01T00:01:{:02}.{:06}Z",
            self.serial % 60,
            self.serial
        );
        let Some(scripts) = self.scripts.get_mut(account) else {
            return refused(403, 10000, "Authentication error");
        };
        let tags = body.as_ref().map(|body| body["tags"].clone());
        if tags
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|tags| tags.len() > 10)
        {
            return refused(
                400,
                100103,
                "You have exceeded the limit of 10 tags per Worker. Remove a tag to add another.",
            );
        }
        match (verb, rest) {
            (Verb::Put, []) => {
                let metadata = body.expect("an upload has metadata");
                let main = metadata["main_module"].as_str().unwrap();
                assert_eq!(main, "worker.js");
                let created = scripts
                    .get(name)
                    .map(|script| script.listed["created_on"].clone())
                    .unwrap_or(json!(now));
                let module = module.expect("an upload has a module");
                let listed = json!({
                    "id": name,
                    "tags": metadata["tags"],
                    "compatibility_date": metadata["compatibility_date"],
                    "etag": format!("{:x}", module.len()),
                    "created_on": created,
                    "modified_on": now,
                });
                scripts.insert(
                    name.to_owned(),
                    Script {
                        listed: listed.clone(),
                        module,
                    },
                );
                ok(listed, None)
            }
            (Verb::Patch, ["script-settings"]) => {
                let Some(script) = scripts.get_mut(name) else {
                    return refused(404, 10007, "This Worker does not exist on your account.");
                };
                let tags = tags.expect("settings have a body");
                script.listed["tags"] = tags.clone();
                ok(json!({ "tags": tags, "logpush": false }), None)
            }
            (Verb::Delete, []) => match scripts.remove(name) {
                Some(_) => {
                    for routes in self.routes.values_mut() {
                        routes.retain(|route| route["script"] != json!(name));
                    }
                    ok(Value::Null, None)
                }
                None => refused(404, 10007, "This Worker does not exist on your account."),
            },
            _ => refused(405, 10000, "Method not allowed"),
        }
    }

    fn route(&mut self, verb: Verb, zone: &str, id: Option<&str>, body: Option<Value>) -> Answer {
        if !self.records.contains_key(zone) {
            return refused(404, 7003, "Could not route to /zones");
        }
        if self.forbidden.contains(zone) {
            return Ok((403, FORBIDDEN.as_bytes().to_vec()));
        }
        self.serial += 1;
        let serial = self.serial;
        let routes = self.routes.entry(zone.to_owned()).or_default();
        let route = |id: String, body: &Value| json!({ "id": id, "pattern": body["pattern"], "script": body.get("script").cloned().unwrap_or(Value::Null) });
        match (verb, id) {
            (Verb::Post, None) => {
                let body = body.expect("a create has a body");
                if routes
                    .iter()
                    .any(|existing| existing["pattern"] == body["pattern"])
                {
                    return refused(409, 10020, "A route with the same pattern already exists.");
                }
                let created = route(format!("{serial:032x}"), &body);
                routes.push(created.clone());
                ok(created, None)
            }
            (verb, Some(id)) => {
                let Some(index) = routes.iter().position(|route| route["id"] == json!(id)) else {
                    return refused(404, 10021, "Route not found.");
                };
                match verb {
                    Verb::Put => {
                        routes[index] = route(id.to_owned(), &body.expect("a replace has a body"));
                        ok(routes[index].clone(), None)
                    }
                    Verb::Delete => {
                        routes.remove(index);
                        ok(json!({ "id": id }), None)
                    }
                    _ => refused(405, 10000, "Method not allowed"),
                }
            }
            (_, None) => refused(405, 10000, "Method not allowed"),
        }
    }
}

/// the metadata and the main module of a worker upload
fn multipart(payload: &Payload) -> (Value, Vec<u8>) {
    let boundary = payload
        .content_type
        .split_once("boundary=")
        .expect("a multipart body names its boundary")
        .1;
    let delimiter = format!("--{boundary}");
    let text = &payload.bytes;
    let mut parts = Vec::new();
    let mut at = 0;
    while let Some(start) = find(&text[at..], delimiter.as_bytes()) {
        let start = at + start + delimiter.len();
        if text[start..].starts_with(b"--") {
            break;
        }
        let next =
            start + find(&text[start..], delimiter.as_bytes()).expect("every part is closed");
        let part = &text[start + 2..next - 2];
        let split = find(part, b"\r\n\r\n").expect("a part has headers");
        let headers = String::from_utf8_lossy(&part[..split]).to_string();
        let name = headers
            .split("name=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .to_owned();
        parts.push((name, part[split + 4..].to_vec()));
        at = next;
    }
    let part = |wanted: &str| {
        parts
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, body)| body.clone())
            .unwrap_or_else(|| panic!("no {wanted} part"))
    };
    let metadata: Value = serde_json::from_slice(&part("metadata")).unwrap();
    let module = part(metadata["main_module"].as_str().unwrap());
    (metadata, module)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
