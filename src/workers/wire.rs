use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::value::Script;
use crate::cloudflare::Payload;

/// the file name the uploaded module goes by, and the metadata's main module
pub(super) const MODULE: &str = "worker.js";

#[derive(Deserialize)]
pub(super) struct LiveScript {
    pub id: String,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub created_on: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
pub(super) struct LiveRoute {
    pub id: String,
    pub pattern: String,
    #[serde(default)]
    pub script: Option<String>,
}

#[derive(Serialize)]
pub(super) struct RouteBody<'a> {
    pub pattern: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<&'a str>,
}

#[derive(Serialize)]
pub(super) struct Settings {
    pub tags: Vec<String>,
}

#[derive(Serialize)]
struct Metadata<'a> {
    main_module: &'a str,
    compatibility_date: String,
    tags: Vec<String>,
}

/// the multipart body that uploads `script` as an es module worker
pub(super) fn upload(script: &Script, tags: Vec<String>) -> Payload {
    let source = script
        .module
        .source
        .as_deref()
        .expect("only a declared script is uploaded");
    let metadata = serde_json::to_vec(&Metadata {
        main_module: MODULE,
        compatibility_date: script.compatibility_date.to_string(),
        tags,
    })
    .expect("the metadata serializes");
    let mut boundary = format!("norn-{}", script.module.digest);
    while contains(source, boundary.as_bytes()) {
        boundary.push('-');
    }
    let mut bytes = Vec::new();
    for (name, content_type, body) in [
        ("metadata", "application/json", metadata.as_slice()),
        (MODULE, "application/javascript+module", source),
    ] {
        bytes.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n"
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Payload {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        bytes,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
