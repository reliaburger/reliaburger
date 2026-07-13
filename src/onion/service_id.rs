/// Namespace-qualified service identity for Onion.
///
/// A service is identified by its namespace *and* its name, never the
/// bare name alone. Two apps called `api` in `default` and `payments`
/// are different services with different VIPs, DNS names and routes.
/// Keying the service map on the bare name was the D3/codex-M1 collision
/// bug: the second `api` overwrote the first.
///
/// The canonical string form is `{namespace}__{name}`, matching Grill's
/// [`InstanceIdentity`](crate::grill::InstanceIdentity). Both parts are
/// DNS-1123 labels (`[a-z0-9-]` only), so the `__` separator can never
/// appear inside either half and the split back out is unambiguous.
use std::fmt;

/// The separator between the namespace and the service name in a
/// canonical service id. Two underscores because a single underscore is
/// illegal in a DNS-1123 label, so this can't collide with any real
/// namespace or service name. Kept identical to Grill's instance-id
/// separator so the two id schemes read the same way.
const NAMESPACE_SEPARATOR: &str = "__";

/// The namespace-qualified identity of a service.
///
/// Use this as the map key rather than a bare `String`: the compiler
/// then prevents accidentally looking a service up by name without a
/// namespace, which is exactly the mix-up that caused the collision.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ServiceId {
    /// Namespace the service lives in (`default`, `payments`, …).
    pub namespace: String,
    /// Service (app) name.
    pub name: String,
}

impl ServiceId {
    /// Build a service id from its namespace and name.
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
        }
    }

    /// The canonical, namespace-qualified string form
    /// (`{namespace}__{name}`). This is what the VIP hash, the map key
    /// and any on-wire representation use.
    pub fn qualified(&self) -> String {
        format!("{}{NAMESPACE_SEPARATOR}{}", self.namespace, self.name)
    }

    /// Parse a canonical qualified string back into a `ServiceId`.
    ///
    /// Returns `None` for a bare (namespace-less) name, which is not a
    /// valid service id under this theme.
    pub fn parse(qualified: &str) -> Option<Self> {
        let (namespace, name) = qualified.split_once(NAMESPACE_SEPARATOR)?;
        if namespace.is_empty() || name.is_empty() {
            return None;
        }
        Some(Self::new(namespace, name))
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.qualified())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_is_namespace_then_name() {
        let id = ServiceId::new("default", "api");
        assert_eq!(id.qualified(), "default__api");
    }

    #[test]
    fn same_name_two_namespaces_differ() {
        let a = ServiceId::new("default", "api");
        let b = ServiceId::new("payments", "api");
        assert_ne!(a, b);
        assert_ne!(a.qualified(), b.qualified());
    }

    #[test]
    fn parse_round_trips() {
        let id = ServiceId::new("payments", "api");
        let parsed = ServiceId::parse(&id.qualified()).expect("parses");
        assert_eq!(parsed, id);
    }

    #[test]
    fn parse_hyphenated_name() {
        let id = ServiceId::new("team-a", "my-web-app");
        let parsed = ServiceId::parse(&id.qualified()).expect("parses");
        assert_eq!(parsed, id);
        assert_eq!(parsed.namespace, "team-a");
        assert_eq!(parsed.name, "my-web-app");
    }

    #[test]
    fn parse_rejects_bare_name() {
        assert!(ServiceId::parse("api").is_none());
    }

    #[test]
    fn parse_rejects_empty_halves() {
        assert!(ServiceId::parse("__api").is_none());
        assert!(ServiceId::parse("default__").is_none());
    }

    #[test]
    fn matches_instance_identity_separator() {
        // The service id and the Grill instance id share the `__`
        // namespace separator, so both read the same way.
        let sid = ServiceId::new("payments", "api").qualified();
        let iid = crate::grill::InstanceIdentity::new("payments", "api", 0)
            .instance_id()
            .0;
        assert!(sid.starts_with("payments__"));
        assert!(iid.starts_with("payments__"));
    }
}
