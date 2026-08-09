//! Server-owned permission policy for Phase 15 operations.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::sesame::types::ApiRole;

/// Absolute ceiling for a Phase 15 resource lease.
pub const MAX_TEST_LEASE_SECONDS: u64 = 24 * 60 * 60;
/// Hard CPU ceiling for a node-pressure helper.
pub const MAX_NODE_PRESSURE_CPU_PERCENT: u8 = 90;
/// Hard memory-usage target ceiling for a node-pressure helper.
pub const MAX_NODE_PRESSURE_MEMORY_PERCENT: u8 = 90;

/// Operator-declared cluster class. Unknown is protected, not development.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterSafetyClass {
    #[default]
    Unknown,
    Development,
    Staging,
    Production,
}

impl ClusterSafetyClass {
    /// Whether mutation needs the explicit protected-cluster server gate.
    pub fn is_protected(self) -> bool {
        matches!(self, Self::Unknown | Self::Production)
    }
}

/// Independently authorised Phase 15 operation classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPermission {
    ReadDiagnostics,
    ProvisionIsolatedWorkloads,
    InjectWorkloadFaults,
    AlterNodeState,
    SaturateCapacity,
    ProbeExternalDestination,
}

impl std::fmt::Display for OperationPermission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ReadDiagnostics => "read_diagnostics",
            Self::ProvisionIsolatedWorkloads => "provision_isolated_workloads",
            Self::InjectWorkloadFaults => "inject_workload_faults",
            Self::AlterNodeState => "alter_node_state",
            Self::SaturateCapacity => "saturate_capacity",
            Self::ProbeExternalDestination => "probe_external_destination",
        })
    }
}

impl OperationPermission {
    /// Lowest API role which may request this operation.
    pub fn minimum_role(self) -> ApiRole {
        match self {
            Self::ReadDiagnostics => ApiRole::ReadOnly,
            Self::ProvisionIsolatedWorkloads | Self::InjectWorkloadFaults => ApiRole::Deployer,
            Self::AlterNodeState | Self::SaturateCapacity | Self::ProbeExternalDestination => {
                ApiRole::Admin
            }
        }
    }

    fn needs_acknowledgement(self) -> bool {
        matches!(
            self,
            Self::InjectWorkloadFaults | Self::AlterNodeState | Self::SaturateCapacity
        )
    }

    fn mutates_or_probes(self) -> bool {
        self != Self::ReadDiagnostics
    }
}

/// Server-side test policy. Client flags cannot expand it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterTestPolicy {
    /// Typed operator declaration. Missing configuration remains `Unknown`.
    pub safety_class: ClusterSafetyClass,
    /// Operation classes this server will accept.
    pub allowed_operations: BTreeSet<OperationPermission>,
    /// Additional server-side gate for mutation on protected clusters.
    pub allow_protected_mutation: bool,
    /// Longest resource lease this server may issue.
    pub max_lease_seconds: u64,
    /// Largest total-node CPU percentage a pressure fault may consume.
    ///
    /// Zero disables CPU pressure. The default is zero, so capacity
    /// saturation is opt-in even when the operation permission is present.
    pub max_node_pressure_cpu_percent: u8,
    /// Largest total-node memory-usage target a pressure fault may request.
    ///
    /// Zero disables memory pressure. The 90% hard ceiling preserves a
    /// control-plane and kernel headroom band.
    pub max_node_pressure_memory_percent: u8,
    /// Destinations trace may probe when the caller also has permission.
    pub external_probe_allowlist: Vec<String>,
}

impl Default for ClusterTestPolicy {
    fn default() -> Self {
        Self {
            safety_class: ClusterSafetyClass::Unknown,
            allowed_operations: BTreeSet::new(),
            allow_protected_mutation: false,
            max_lease_seconds: 3_600,
            max_node_pressure_cpu_percent: 0,
            max_node_pressure_memory_percent: 0,
            external_probe_allowlist: Vec::new(),
        }
    }
}

/// Authenticated caller facts. Acknowledgement records consent, not authority.
#[derive(Debug, Clone, Copy)]
pub struct OperationAuthorisation<'a> {
    /// Authenticated identity used in audit evidence.
    pub principal: &'a str,
    /// Role established by the API authentication layer.
    pub role: ApiRole,
    /// Explicit operator consent; this grants no authority on its own.
    pub acknowledged: bool,
}

/// Auditable result of an accepted safety decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorisationDecision {
    /// Authenticated caller.
    pub principal: String,
    /// Operation the server accepted.
    pub operation: OperationPermission,
    /// Safety class used for the decision.
    pub safety_class: ClusterSafetyClass,
}

/// Why the server refused a Phase 15 operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SafetyError {
    #[error("authenticated role is not permitted to perform this operation")]
    InsufficientRole,
    #[error("cluster policy does not allow this operation")]
    OperationNotAllowed,
    #[error("cluster is protected and protected-cluster mutation is disabled")]
    ProtectedCluster,
    #[error("destructive operation requires acknowledgement")]
    AcknowledgementRequired,
    /// A zero or effectively permanent lease limit is unsafe.
    #[error("max lease lifetime must be between 1 and 86400 seconds")]
    InvalidLeaseLimit,
    #[error("max node-pressure CPU must be between 0 and 90 percent")]
    InvalidNodePressureCpuLimit,
    #[error("max node-pressure memory must be between 0 and 90 percent")]
    InvalidNodePressureMemoryLimit,
}

impl ClusterTestPolicy {
    /// Validate server-owned resource lifetime bounds.
    pub fn validate(&self) -> Result<(), SafetyError> {
        if self.max_lease_seconds == 0 || self.max_lease_seconds > MAX_TEST_LEASE_SECONDS {
            return Err(SafetyError::InvalidLeaseLimit);
        }
        if self.max_node_pressure_cpu_percent > MAX_NODE_PRESSURE_CPU_PERCENT {
            return Err(SafetyError::InvalidNodePressureCpuLimit);
        }
        if self.max_node_pressure_memory_percent > MAX_NODE_PRESSURE_MEMORY_PERCENT {
            return Err(SafetyError::InvalidNodePressureMemoryLimit);
        }
        Ok(())
    }

    /// Whether an external trace destination is explicitly allowlisted.
    /// Matching is exact and includes the port; there are no wildcards or DNS
    /// suffix rules that could silently expand probe authority.
    pub fn permits_external_probe(&self, host: &str, port: u16) -> bool {
        let authority = match host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(address)) => format!("[{address}]:{port}"),
            _ => format!("{host}:{port}"),
        };
        self.external_probe_allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&authority))
    }

    /// Fail closed unless role, server policy, protection and consent agree.
    pub fn authorise(
        &self,
        operation: OperationPermission,
        caller: &OperationAuthorisation<'_>,
    ) -> Result<AuthorisationDecision, SafetyError> {
        crate::sesame::token::check_role(caller.role, operation.minimum_role())
            .map_err(|_| SafetyError::InsufficientRole)?;
        if operation.mutates_or_probes() && !self.allowed_operations.contains(&operation) {
            return Err(SafetyError::OperationNotAllowed);
        }
        if operation.mutates_or_probes()
            && self.safety_class.is_protected()
            && !self.allow_protected_mutation
        {
            return Err(SafetyError::ProtectedCluster);
        }
        if operation.needs_acknowledgement() && !caller.acknowledged {
            return Err(SafetyError::AcknowledgementRequired);
        }
        Ok(AuthorisationDecision {
            principal: caller.principal.to_string(),
            operation,
            safety_class: self.safety_class,
        })
    }

    /// Authorise reversal of an already-active destructive operation.
    ///
    /// Reversal still needs the operation's role and explicit server grant,
    /// but never needs acknowledgement or the protected-cluster mutation
    /// switch. Those gates exist to stop escalation. Requiring them to remove
    /// an active fault could trap the cluster in the dangerous state.
    pub fn authorise_reversal(
        &self,
        operation: OperationPermission,
        caller: &OperationAuthorisation<'_>,
    ) -> Result<AuthorisationDecision, SafetyError> {
        crate::sesame::token::check_role(caller.role, operation.minimum_role())
            .map_err(|_| SafetyError::InsufficientRole)?;
        if !self.allowed_operations.contains(&operation) {
            return Err(SafetyError::OperationNotAllowed);
        }
        Ok(AuthorisationDecision {
            principal: caller.principal.to_string(),
            operation,
            safety_class: self.safety_class,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(role: ApiRole, acknowledged: bool) -> OperationAuthorisation<'static> {
        OperationAuthorisation {
            principal: "ci",
            role,
            acknowledged,
        }
    }

    #[test]
    fn unknown_cluster_and_yes_flag_never_grant_mutation() {
        let policy = ClusterTestPolicy::default();
        assert_eq!(
            policy.authorise(
                OperationPermission::AlterNodeState,
                &caller(ApiRole::Admin, true)
            ),
            Err(SafetyError::OperationNotAllowed)
        );
        assert!(policy.safety_class.is_protected());
    }

    #[test]
    fn protected_mutation_needs_both_server_gates() {
        let mut policy = ClusterTestPolicy {
            safety_class: ClusterSafetyClass::Production,
            allowed_operations: BTreeSet::from([OperationPermission::AlterNodeState]),
            ..ClusterTestPolicy::default()
        };
        assert_eq!(
            policy.authorise(
                OperationPermission::AlterNodeState,
                &caller(ApiRole::Admin, true)
            ),
            Err(SafetyError::ProtectedCluster)
        );
        policy.allow_protected_mutation = true;
        assert!(
            policy
                .authorise(
                    OperationPermission::AlterNodeState,
                    &caller(ApiRole::Admin, true)
                )
                .is_ok()
        );
    }

    #[test]
    fn reversal_needs_the_original_grant_but_not_escalation_consent() {
        let policy = ClusterTestPolicy {
            safety_class: ClusterSafetyClass::Production,
            allowed_operations: BTreeSet::from([OperationPermission::InjectWorkloadFaults]),
            allow_protected_mutation: false,
            ..ClusterTestPolicy::default()
        };
        assert!(
            policy
                .authorise_reversal(
                    OperationPermission::InjectWorkloadFaults,
                    &caller(ApiRole::Deployer, false),
                )
                .is_ok()
        );
        assert_eq!(
            policy.authorise_reversal(
                OperationPermission::AlterNodeState,
                &caller(ApiRole::Admin, true),
            ),
            Err(SafetyError::OperationNotAllowed)
        );
        assert_eq!(
            policy.authorise_reversal(
                OperationPermission::InjectWorkloadFaults,
                &caller(ApiRole::ReadOnly, true),
            ),
            Err(SafetyError::InsufficientRole)
        );
    }

    #[test]
    fn node_pressure_is_disabled_by_default_and_hard_capped() {
        let mut policy = ClusterTestPolicy::default();
        assert_eq!(policy.max_node_pressure_cpu_percent, 0);
        assert_eq!(policy.max_node_pressure_memory_percent, 0);

        policy.max_node_pressure_cpu_percent = MAX_NODE_PRESSURE_CPU_PERCENT + 1;
        assert_eq!(
            policy.validate(),
            Err(SafetyError::InvalidNodePressureCpuLimit)
        );
        policy.max_node_pressure_cpu_percent = MAX_NODE_PRESSURE_CPU_PERCENT;
        policy.max_node_pressure_memory_percent = MAX_NODE_PRESSURE_MEMORY_PERCENT + 1;
        assert_eq!(
            policy.validate(),
            Err(SafetyError::InvalidNodePressureMemoryLimit)
        );
    }

    #[test]
    fn external_probe_allowlist_is_exact_and_port_scoped() {
        let mut policy = ClusterTestPolicy::default();
        assert!(!policy.permits_external_probe("example.com", 443));
        policy.external_probe_allowlist = vec![
            "example.com:443".to_string(),
            "[2001:db8::1]:8443".to_string(),
        ];
        assert!(policy.permits_external_probe("EXAMPLE.COM", 443));
        assert!(!policy.permits_external_probe("example.com", 80));
        assert!(!policy.permits_external_probe("sub.example.com", 443));
        assert!(policy.permits_external_probe("2001:db8::1", 8443));
    }
}
