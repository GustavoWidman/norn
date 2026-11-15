//! the http layer, private to `cloudflare`. a [Reader] holds something that
//! can only GET; only a [Sender] holds something that can send another
//! method, and only an `Api<Write>` holds a [Sender].

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, de::DeserializeOwned};
use zeroize::Zeroizing;

use super::{ApiError, Verb};

const BASE: &str = "https://api.cloudflare.com/client/v4";
const PER_PAGE: &str = "500";

/// the run's time budget. every request gets the smaller of `per_request`
/// and what is left, and once nothing is left a request fails with
/// [ApiError::Expired] without being sent. callers never handle time: past
/// the deadline their calls fail fast, and the run records what did not apply.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    ends: Instant,
    per_request: Duration,
}

impl Clock {
    pub fn start(deadline: Duration, per_request: Duration) -> Self {
        Self {
            ends: Instant::now() + deadline,
            per_request,
        }
    }

    fn budget(&self) -> Result<Duration, ApiError> {
        let left = self
            .ends
            .checked_duration_since(Instant::now())
            .ok_or(ApiError::Expired)?;
        if left.is_zero() {
            return Err(ApiError::Expired);
        }
        Ok(left.min(self.per_request))
    }
}

/// a status and a body, or why there is none
pub type Answer = Result<(u16, Vec<u8>), ApiError>;

/// sends GETs: what a [Reader] holds. `path` is relative to `/client/v4`.
pub trait Fetch {
    fn get(&self, path: &str, query: &[(&str, &str)], token: &str, timeout: Duration) -> Answer;
}

/// sends every other method: what only a [Sender] holds
pub trait Deliver {
    fn send(
        &self,
        verb: Verb,
        path: &str,
        body: Option<Vec<u8>>,
        token: &str,
        timeout: Duration,
    ) -> Answer;
}

/// cloudflare over https
pub struct Https(ureq::Agent);

impl Https {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!("norn/", env!("CARGO_PKG_VERSION")))
            .build();
        Self(ureq::Agent::new_with_config(config))
    }

    fn answer(
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        timeout: Duration,
    ) -> Answer {
        let failed = |error: ureq::Error| match error {
            ureq::Error::Timeout(_) => ApiError::TimedOut(timeout),
            other => ApiError::Unreachable(other.to_string()),
        };
        let mut response = result.map_err(failed)?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_vec().map_err(failed)?;
        Ok((status, body))
    }
}

impl Fetch for Https {
    fn get(&self, path: &str, query: &[(&str, &str)], token: &str, timeout: Duration) -> Answer {
        let mut request = self.0.get(format!("{BASE}{path}"));
        for (key, value) in query {
            request = request.query(*key, *value);
        }
        let request = request
            .header("Authorization", format!("Bearer {token}"))
            .config()
            .timeout_global(Some(timeout))
            .build();
        Self::answer(request.call(), timeout)
    }
}

impl Deliver for Https {
    fn send(
        &self,
        verb: Verb,
        path: &str,
        body: Option<Vec<u8>>,
        token: &str,
        timeout: Duration,
    ) -> Answer {
        let url = format!("{BASE}{path}");
        let auth = format!("Bearer {token}");
        let with_body = |request: ureq::RequestBuilder<ureq::typestate::WithBody>| {
            let request = request
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .config()
                .timeout_global(Some(timeout))
                .build();
            request.send(body.clone().unwrap_or_default())
        };
        let result = match verb {
            Verb::Post => with_body(self.0.post(&url)),
            Verb::Put => with_body(self.0.put(&url)),
            Verb::Patch => with_body(self.0.patch(&url)),
            Verb::Delete => self
                .0
                .delete(&url)
                .header("Authorization", &auth)
                .config()
                .timeout_global(Some(timeout))
                .build()
                .call(),
        };
        Self::answer(result, timeout)
    }
}

pub struct Reader {
    fetch: Arc<dyn Fetch>,
    token: Arc<Zeroizing<String>>,
    clock: Clock,
}

pub struct Sender {
    deliver: Arc<dyn Deliver>,
    token: Arc<Zeroizing<String>>,
    clock: Clock,
}

impl Reader {
    pub fn new(fetch: Arc<dyn Fetch>, token: Arc<Zeroizing<String>>, clock: Clock) -> Self {
        Self {
            fetch,
            token,
            clock,
        }
    }

    pub fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, ApiError> {
        let timeout = self.clock.budget()?;
        let (status, body) = self.fetch.get(path, query, &self.token, timeout)?;
        Ok(envelope::<T>(status, &body)?.0)
    }

    /// every page of a listing, following `result_info`; one failed page
    /// fails the whole listing, so a partial listing never reads as complete
    pub fn list<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>, ApiError> {
        let mut all = Vec::new();
        for page in 1.. {
            let page = page.to_string();
            let mut paged = query.to_vec();
            paged.extend([("page", page.as_str()), ("per_page", PER_PAGE)]);
            let timeout = self.clock.budget()?;
            let (status, body) = self.fetch.get(path, &paged, &self.token, timeout)?;
            let (items, info) = envelope::<Vec<T>>(status, &body)?;
            let done = items.is_empty() || info.is_none_or(|info| info.page >= info.total_pages);
            all.extend(items);
            if done {
                break;
            }
        }
        Ok(all)
    }
}

impl Sender {
    pub fn new(deliver: Arc<dyn Deliver>, token: Arc<Zeroizing<String>>, clock: Clock) -> Self {
        Self {
            deliver,
            token,
            clock,
        }
    }

    /// no retries: a write that timed out may have landed, and the next run
    /// reads what is there before deciding again
    pub fn send<T: DeserializeOwned>(
        &self,
        verb: Verb,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<T, ApiError> {
        let timeout = self.clock.budget()?;
        let (status, body) = self.deliver.send(verb, path, body, &self.token, timeout)?;
        Ok(envelope::<T>(status, &body)?.0)
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<Message>,
    result: Option<T>,
    result_info: Option<ResultInfo>,
}

#[derive(Deserialize)]
struct Message {
    code: u32,
    message: String,
}

#[derive(Deserialize, Clone, Copy)]
struct ResultInfo {
    #[serde(default)]
    page: u32,
    #[serde(default)]
    total_pages: u32,
}

/// `{success, errors, result, result_info}` -> `result`, or the reason it is missing.
/// 404 is [ApiError::NotFound], so a delete of what is already gone reads as done.
fn envelope<T: DeserializeOwned>(
    status: u16,
    body: &[u8],
) -> Result<(T, Option<ResultInfo>), ApiError> {
    if status == 404 {
        return Err(ApiError::NotFound);
    }
    let envelope: Envelope<serde_json::Value> = serde_json::from_slice(body)
        .map_err(|error| ApiError::Garbled(format!("status {status}: {error}")))?;
    if !envelope.success || !(200..300).contains(&status) {
        let errors = envelope
            .errors
            .into_iter()
            .map(|error| (error.code, error.message))
            .collect();
        return Err(ApiError::Rejected { status, errors });
    }
    let result = serde_json::from_value(envelope.result.unwrap_or(serde_json::Value::Null))
        .map_err(|error| ApiError::Garbled(error.to_string()))?;
    Ok((result, envelope.result_info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejection_carries_cloudflares_codes() {
        let body = br#"{"success":false,"errors":[{"code":81058,"message":"An identical record already exists."}],"result":null}"#;
        match envelope::<serde_json::Value>(400, body) {
            Err(ApiError::Rejected {
                status: 400,
                errors,
            }) => assert_eq!(errors[0].0, 81058),
            other => panic!("{:?}", other.map(|(value, _)| value)),
        }
    }

    #[test]
    fn a_spent_clock_sends_nothing() {
        let clock = Clock::start(Duration::ZERO, Duration::from_secs(5));
        assert!(matches!(clock.budget(), Err(ApiError::Expired)));
        let clock = Clock::start(Duration::from_secs(30), Duration::from_secs(5));
        assert!(clock.budget().unwrap() <= Duration::from_secs(5));
    }
}
