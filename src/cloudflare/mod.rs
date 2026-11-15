//! the cloudflare api boundary: auth, the `{success, errors, result,
//! result_info}` envelope, pagination, timeouts and the run's deadline. kinds
//! name paths and bring their own wire types; nothing else about http leaves
//! this module.
//!
//! what a client may do is its type. an [`Api<Read>`] holds no sender at
//! all, not a sender behind a flag, and building any client consumes the
//! [Token], so once a run has built a reader it cannot build a writer.
//!
//! a reader cannot send:
//!
//! ```compile_fail,E0599
//! use norn::cloudflare::{Api, Read, Verb};
//!
//! fn delete(reader: &Api<Read>) {
//!     let _: Result<(), _> = reader.send(Verb::Delete, "/zones/z/dns_records/r", None::<&()>);
//! }
//! ```
//!
//! and nothing can build a writer after the reader, because the reader took
//! the only token:
//!
//! ```compile_fail,E0382
//! use norn::cloudflare::{Api, Clock, Token};
//!
//! fn both(token: Token, clock: Clock) {
//!     let reader = Api::read_only(token, clock);
//!     let writer = Api::read_write(token, clock);
//! }
//! ```
//!
//! while a writer, built from its own token, can:
//!
//! ```no_run
//! use norn::cloudflare::{Api, Clock, Token, Verb};
//!
//! fn delete(token: Token, clock: Clock) {
//!     let writer = Api::read_write(token, clock);
//!     let _: Result<(), _> = writer.send(Verb::Delete, "/zones/z/dns_records/r", None::<&()>);
//! }
//! ```

mod http;

use std::{fmt, io, marker::PhantomData, path::Path, sync::Arc};

use serde::{Serialize, de::DeserializeOwned};
use zeroize::Zeroizing;

pub use http::Clock;
#[cfg(test)]
pub(crate) use http::{Answer, Deliver, Fetch};

mod sealed {
    pub trait Sealed {}
}

/// what a client may do; sealed, so the two below are all there are
pub trait Capability: sealed::Sealed {
    /// the means to send writes: nothing for [Read], a sender for [Write]
    type Sender;
}

pub enum Read {}
pub enum Write {}

impl sealed::Sealed for Read {}
impl sealed::Sealed for Write {}

impl Capability for Read {
    type Sender = ();
}

impl Capability for Write {
    type Sender = http::Sender;
}

pub struct Api<C: Capability> {
    reader: http::Reader,
    sender: C::Sender,
    _capability: PhantomData<C>,
}

impl Api<Read> {
    /// consumes the token: once a read-only client exists, nothing in the
    /// process holds a token a writer could be built from
    pub fn read_only(token: Token, clock: Clock) -> Self {
        Self::over_fetch(Arc::new(http::Https::new()), token, clock)
    }

    pub(crate) fn over_fetch(fetch: Arc<dyn http::Fetch>, token: Token, clock: Clock) -> Self {
        Self {
            reader: http::Reader::new(fetch, Arc::new(token.0), clock),
            sender: (),
            _capability: PhantomData,
        }
    }
}

impl Api<Write> {
    pub fn read_write(token: Token, clock: Clock) -> Self {
        Self::over(Arc::new(http::Https::new()), token, clock)
    }

    pub(crate) fn over<T: http::Fetch + http::Deliver + 'static>(
        transport: Arc<T>,
        token: Token,
        clock: Clock,
    ) -> Self {
        let token = Arc::new(token.0);
        Self {
            reader: http::Reader::new(transport.clone(), token.clone(), clock),
            sender: http::Sender::new(transport, token, clock),
            _capability: PhantomData,
        }
    }

    /// one write. `path` is relative to `/client/v4`.
    pub fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        verb: Verb,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, ApiError> {
        let body = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|error| ApiError::Garbled(error.to_string()))?;
        self.sender.send(verb, path, body)
    }
}

impl<C: Capability> Api<C> {
    /// one `result`, e.g. a zone lookup
    pub(crate) fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, ApiError> {
        self.reader.get(path, query)
    }

    /// every page of a listing
    pub(crate) fn list<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>, ApiError> {
        self.reader.list(path, query)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verb {
    Post,
    Put,
    Patch,
    Delete,
}

/// the fleet's api token. neither `Clone` nor `Copy`, and zeroed on drop:
/// read once, then moved into the one client the run builds.
pub struct Token(Zeroizing<String>);

#[derive(Debug)]
pub enum TokenError {
    /// no file at the path, as before agenix first decrypts it
    Missing,
    Unreadable(io::Error),
    Empty,
}

impl Token {
    /// the file's content with surrounding whitespace trimmed
    pub fn read(path: &Path) -> Result<Self, TokenError> {
        let content =
            Zeroizing::new(
                std::fs::read_to_string(path).map_err(|error| match error.kind() {
                    io::ErrorKind::NotFound => TokenError::Missing,
                    _ => TokenError::Unreadable(error),
                })?,
            );
        let token = content.trim();
        if token.is_empty() {
            return Err(TokenError::Empty);
        }
        Ok(Self(Zeroizing::new(token.to_owned())))
    }
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Missing => f.write_str("the token file does not exist"),
            TokenError::Unreadable(error) => write!(f, "the token file cannot be read: {error}"),
            TokenError::Empty => f.write_str("the token file is empty"),
        }
    }
}

/// why a request did not do what it was asked. every variant is something a
/// later run may not hit again; none of them ends a run early.
#[derive(Clone, Debug)]
pub enum ApiError {
    /// the run's deadline had passed; the request was not sent
    Expired,
    /// the request outlived its own timeout
    TimedOut(std::time::Duration),
    /// no connection, a tls or a name resolution failure
    Unreachable(String),
    /// cloudflare answered `success: false`; its codes and messages, verbatim
    Rejected {
        status: u16,
        errors: Vec<(u32, String)>,
    },
    NotFound,
    /// a lookup that must find exactly one object found none, e.g. a zone the token cannot see
    Missing(String),
    /// the response did not parse
    Garbled(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Expired => {
                f.write_str("the run's deadline passed before the request was sent")
            }
            ApiError::TimedOut(after) => write!(f, "no answer within {after:?}"),
            ApiError::Unreachable(why) => write!(f, "cloudflare is unreachable: {why}"),
            ApiError::Rejected { status, errors } => {
                write!(f, "cloudflare refused it ({status})")?;
                for (code, message) in errors {
                    write!(f, ": {code} {message}")?;
                }
                Ok(())
            }
            ApiError::NotFound => f.write_str("not found"),
            ApiError::Missing(what) => write!(f, "{what} not found"),
            ApiError::Garbled(why) => write!(f, "cloudflare's answer did not parse: {why}"),
        }
    }
}
