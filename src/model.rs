use std::{cmp::Ordering, collections::BTreeSet, fmt};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct HostName(String);

impl HostName {
    pub fn parse(name: &str) -> Result<Self, String> {
        if name == "fleet" {
            return Err("`fleet` names the fleet's own mark and cannot be a machine's name".into());
        }
        if name.is_empty() || name.len() > 63 {
            return Err(format!("{name:?} is not 1 to 63 bytes long"));
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(format!("{name:?} has a character outside [a-z0-9-]"));
        }
        if name.starts_with('-') || name.ends_with('-') {
            return Err(format!("{name:?} starts or ends with `-`"));
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) enum Owner {
    Fleet,
    Host(HostName),
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Owner::Fleet => f.write_str("fleet"),
            Owner::Host(host) => f.write_str(host.as_str()),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Owners(BTreeSet<Owner>);

impl Owners {
    pub fn machine(host: HostName) -> Self {
        Self(BTreeSet::from([Owner::Host(host), Owner::Fleet]))
    }

    pub fn contains(&self, owner: &Owner) -> bool {
        self.0.contains(owner)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Fqdn(String);

impl Fqdn {
    pub fn parse(name: &str) -> Result<Self, String> {
        if name.is_empty() || name.len() > 253 {
            return Err(format!("{name:?} is not 1 to 253 bytes long"));
        }
        for (index, label) in name.split('.').enumerate() {
            if label == "*" && index == 0 {
                continue;
            }
            if label.is_empty() || label.len() > 63 {
                return Err(format!(
                    "{name:?} has a label that is not 1 to 63 bytes long"
                ));
            }
            let allowed =
                |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_';
            if !label.bytes().all(allowed) {
                return Err(format!(
                    "{name:?} is not in canonical form: lowercase, no trailing dot, labels of [a-z0-9_-]"
                ));
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(format!("{name:?} has a label that starts or ends with `-`"));
            }
        }
        Ok(Self(name.to_owned()))
    }

    pub fn from_live(name: &str) -> Option<Self> {
        let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
        Self::parse(&name).ok()
    }

    pub fn is_within(&self, zone: &Zone) -> bool {
        let zone = zone.name().as_str();
        self.0 == zone
            || self
                .0
                .strip_suffix(zone)
                .is_some_and(|rest| rest.ends_with('.'))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Ord for Fqdn {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.rsplit('.').cmp(other.0.rsplit('.'))
    }
}

impl PartialOrd for Fqdn {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Fqdn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct Zone(Fqdn);

impl Zone {
    pub fn parse(name: &str) -> Result<Self, String> {
        let name = Fqdn::parse(name)?;
        if name.as_str().starts_with("*.") {
            return Err(format!("zone {name} cannot be a wildcard"));
        }
        Ok(Self(name))
    }

    pub fn name(&self) -> &Fqdn {
        &self.0
    }
}

impl fmt::Display for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Zones(BTreeSet<Zone>);

impl Zones {
    pub fn parse(names: &[String]) -> Result<Self, Vec<String>> {
        let mut zones = BTreeSet::new();
        let mut problems = Vec::new();
        for name in names {
            match Zone::parse(name) {
                Ok(zone) => {
                    if !zones.insert(zone) {
                        problems.push(format!("zone {name} is listed twice"));
                    }
                }
                Err(problem) => problems.push(problem),
            }
        }
        if names.is_empty() {
            problems.push("no zones: cloudflare.zones lists the zones the fleet manages".into());
        }
        if problems.is_empty() {
            Ok(Self(zones))
        } else {
            Err(problems)
        }
    }

    pub fn zone_of(&self, name: &Fqdn) -> Option<&Zone> {
        self.0
            .iter()
            .filter(|zone| name.is_within(zone))
            .max_by_key(|zone| zone.name().as_str().len())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Zone> {
        self.0.iter()
    }
}

impl fmt::Display for Zones {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.0.iter().map(|zone| zone.name().as_str()).collect();
        f.write_str(&names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_needs_rewriting_is_refused() {
        assert!(Fqdn::parse("Cache.example.com").is_err());
        assert!(Fqdn::parse("cache.example.com.").is_err());
        assert!(Fqdn::parse("-cache.example.com").is_err());
        assert!(Fqdn::parse("a..example.com").is_err());
        assert!(Fqdn::parse("_dmarc.example.com").is_ok());
        assert!(Fqdn::parse("*.example.com").is_ok());
        assert!(Fqdn::parse("a.*.example.com").is_err());
    }

    #[test]
    fn a_live_name_is_folded_to_the_canonical_spelling() {
        assert_eq!(
            Fqdn::from_live("WWW.Example.com."),
            Fqdn::parse("www.example.com").ok()
        );
    }

    #[test]
    fn names_order_by_reversed_labels() {
        let mut names: Vec<Fqdn> = [
            "s3.cache.example.com",
            "example.com",
            "cache.example.com",
            "a.example.org",
        ]
        .into_iter()
        .map(|name| Fqdn::parse(name).unwrap())
        .collect();
        names.sort();
        let names: Vec<&str> = names.iter().map(Fqdn::as_str).collect();
        assert_eq!(
            names,
            [
                "example.com",
                "cache.example.com",
                "s3.cache.example.com",
                "a.example.org"
            ]
        );
    }

    #[test]
    fn a_name_belongs_to_its_longest_zone() {
        let zones = Zones::parse(&["example.com".into(), "sub.example.com".into()]).unwrap();
        let zone_of = |name: &str| {
            zones
                .zone_of(&Fqdn::parse(name).unwrap())
                .map(|zone| zone.to_string())
        };
        assert_eq!(zone_of("www.example.com").as_deref(), Some("example.com"));
        assert_eq!(
            zone_of("a.sub.example.com").as_deref(),
            Some("sub.example.com")
        );
        assert_eq!(zone_of("badexample.com"), None);
    }

    #[test]
    fn a_machine_named_fleet_is_refused() {
        assert!(HostName::parse("fleet").is_err());
        assert!(HostName::parse("oracle-1").is_ok());
        assert!(HostName::parse("Oracle").is_err());
    }
}
