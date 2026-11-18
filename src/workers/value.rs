use std::{collections::BTreeSet, fmt, sync::Arc};

use chrono::NaiveDate;
use sha2::{Digest as _, Sha256};

use crate::model::{Fqdn, Zone, Zones};

/// a cloudflare account id: 32 lowercase hex digits
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct Account(String);

impl Account {
    pub fn parse(id: &str) -> Result<Self, String> {
        let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if id.len() != 32 || !id.bytes().all(hex) {
            return Err(format!(
                "{id:?} is not an account id, 32 lowercase hex digits"
            ));
        }
        Ok(Self(id.to_owned()))
    }
}

impl fmt::Display for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct ScriptName(String);

impl ScriptName {
    pub fn parse(name: &str) -> Result<Self, String> {
        if name.is_empty() || name.len() > 63 {
            return Err(format!("{name:?} is not 1 to 63 bytes long"));
        }
        let allowed =
            |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_';
        if !name.bytes().all(allowed) {
            return Err(format!("{name:?} has a character outside [a-z0-9_-]"));
        }
        if name.starts_with(['-', '_']) || name.ends_with(['-', '_']) {
            return Err(format!("{name:?} starts or ends with `-` or `_`"));
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ScriptName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// the first half of a sha-256, which is plenty to tell two versions apart
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Digest([u8; 16]);

impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        let full = Sha256::digest(bytes);
        let mut half = [0; 16];
        half.copy_from_slice(&full[..16]);
        Self(half)
    }

    pub fn parse(hex: &str) -> Option<Self> {
        if hex.len() != 32 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        let mut bytes = [0; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * index..2 * index + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }

    fn short(&self) -> String {
        self.to_string()[..12].to_owned()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct CompatibilityDate(NaiveDate);

impl CompatibilityDate {
    pub fn parse(date: &str) -> Result<Self, String> {
        NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .ok()
            .map(Self)
            .filter(|parsed| parsed.to_string() == date)
            .ok_or_else(|| format!("{date:?} is not a date written YYYY-MM-DD"))
    }
}

impl fmt::Display for CompatibilityDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.format("%Y-%m-%d"))
    }
}

/// a worker route's pattern: a host, with an optional leading `*`, and a
/// path, with an optional trailing `*`
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct Pattern(String);

impl Pattern {
    /// a declared pattern, checked against the zones the fleet manages
    pub fn declared(pattern: &str, zones: &Zones) -> Result<Self, String> {
        let parsed = Self(pattern.to_owned());
        let host = parsed.host()?;
        if zones.zone_of(&host).is_none() {
            return Err(format!("{pattern} is outside every managed zone ({zones})"));
        }
        Ok(parsed)
    }

    /// a pattern as cloudflare reports it, taken as it is
    pub fn live(pattern: &str) -> Self {
        Self(pattern.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// the zone a declared pattern belongs to
    pub fn zone<'a>(&self, zones: &'a Zones) -> Option<&'a Zone> {
        zones.zone_of(&self.host().ok()?)
    }

    fn host(&self) -> Result<Fqdn, String> {
        let pattern = &self.0;
        if pattern.contains("://") {
            return Err(format!(
                "{pattern} names a scheme; leave it out, and the route matches http and https alike"
            ));
        }
        let Some((host, path)) = pattern.split_once('/') else {
            return Err(format!(
                "{pattern} has no path; end it in `/*` to match every path"
            ));
        };
        let path = path.strip_suffix('*').unwrap_or(path);
        if path.contains(['*', '?']) || path.chars().any(char::is_whitespace) {
            return Err(format!(
                "{pattern} has a path with a query, a space or a `*` before its end"
            ));
        }
        let bare = match host.strip_prefix('*') {
            Some(rest) => rest.strip_prefix('.').unwrap_or(rest),
            None => host,
        };
        if bare.contains('*') {
            return Err(format!(
                "{pattern} has a `*` inside its host; only a leading one matches"
            ));
        }
        Fqdn::parse(bare).map_err(|why| format!("{pattern}: {why}"))
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// a worker's main module. `source` holds the bytes of a declared module,
/// read once, so what is uploaded is what was compared; a live module is
/// known only by the digest its mark records
#[derive(Clone, Debug)]
pub(crate) struct Module {
    pub digest: Digest,
    pub source: Option<Arc<[u8]>>,
}

impl Module {
    pub fn declared(source: Arc<[u8]>) -> Self {
        Self {
            digest: Digest::of(&source),
            source: Some(source),
        }
    }

    pub fn live(digest: Digest) -> Self {
        Self {
            digest,
            source: None,
        }
    }
}

impl PartialEq for Module {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest
    }
}

impl Eq for Module {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Script {
    pub name: ScriptName,
    pub compatibility_date: CompatibilityDate,
    pub module: Module,
    /// the patterns of the fleet's routes that run it, which are the fleet's
    /// only while it lists them
    pub routes: BTreeSet<Pattern>,
    /// the patterns of the fleet's routes that run no worker, listed with it
    /// for the same reason
    pub bypass: BTreeSet<Pattern>,
}

impl fmt::Display for Script {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "module {}  {}",
            self.module.digest.short(),
            self.compatibility_date
        )?;
        for (name, patterns) in [("routes", &self.routes), ("bypass", &self.bypass)] {
            if !patterns.is_empty() {
                let listed: Vec<&str> = patterns.iter().map(Pattern::as_str).collect();
                write!(f, "  {name} {}", listed.join(" "))?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Route {
    pub pattern: Pattern,
    /// none for a route that bypasses every worker
    pub script: Option<ScriptName>,
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.script {
            Some(script) => write!(f, "runs {script}"),
            None => f.write_str("runs no worker"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zones() -> Zones {
        Zones::parse(&["example.com".into(), "sub.example.com".into()]).unwrap()
    }

    #[test]
    fn a_pattern_belongs_to_the_zone_of_its_host() {
        let zone = |pattern: &str| {
            Pattern::declared(pattern, &zones())
                .map(|parsed| parsed.zone(&zones()).unwrap().to_string())
        };
        assert_eq!(zone("coeus.example.com/*").as_deref(), Ok("example.com"));
        assert_eq!(zone("*example.com/*").as_deref(), Ok("example.com"));
        assert_eq!(
            zone("*.sub.example.com/api/*").as_deref(),
            Ok("sub.example.com")
        );
        assert_eq!(zone("example.com/exact").as_deref(), Ok("example.com"));
    }

    #[test]
    fn a_pattern_cloudflare_would_refuse_or_read_otherwise_is_refused() {
        for (pattern, why) in [
            ("coeus.example.net/*", "outside every managed zone"),
            ("https://coeus.example.com/*", "names a scheme"),
            ("coeus.example.com", "has no path"),
            ("coeus.example.com/*/x", "before its end"),
            ("coeus.example.com/x?y=1", "a query"),
            ("a.*.example.com/*", "inside its host"),
            ("Coeus.example.com/*", "canonical form"),
        ] {
            let refused = Pattern::declared(pattern, &zones()).unwrap_err();
            assert!(refused.contains(why), "{pattern}: {refused}");
        }
    }

    #[test]
    fn a_digest_reads_back() {
        let digest = Digest::of(b"export default {}");
        assert_eq!(Digest::parse(&digest.to_string()), Some(digest));
        assert_eq!(Digest::parse(&digest.to_string().to_uppercase()), None);
    }

    #[test]
    fn names_dates_and_accounts_are_checked() {
        assert!(ScriptName::parse("coeus-offline").is_ok());
        assert!(ScriptName::parse("Coeus").is_err());
        assert!(ScriptName::parse("-coeus").is_err());
        assert!(CompatibilityDate::parse("2026-11-01").is_ok());
        assert!(CompatibilityDate::parse("2026-11-1").is_err());
        assert!(CompatibilityDate::parse("2026-02-30").is_err());
        assert!(Account::parse(&"0a".repeat(16)).is_ok());
        assert!(Account::parse(&"0A".repeat(16)).is_err());
        assert!(Account::parse("0a").is_err());
    }
}
