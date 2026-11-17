use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};

use crate::cloudflare::{Answer, Deliver, Fetch, Verb};

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
    requests: Vec<String>,
    serial: u64,
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
        body: Option<Vec<u8>>,
        _token: &str,
        _timeout: Duration,
    ) -> Answer {
        let interleaved = self.interleaved.lock().unwrap().take();
        if let Some(other) = interleaved {
            other();
        }
        let mut state = self.state.lock().unwrap();
        let body: Option<Value> = body.map(|body| serde_json::from_slice(&body).unwrap());
        let shown = body.as_ref().map(Value::to_string).unwrap_or_default();
        let name = match verb {
            Verb::Post => "POST",
            Verb::Put => "PUT",
            Verb::Patch => "PATCH",
            Verb::Delete => "DELETE",
        };
        state
            .requests
            .push(format!("{name} {path} {shown}").trim_end().to_owned());

        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
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
