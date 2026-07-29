//! Pure NAT observation classifier and traversal plan builder.
//!
//! This module intentionally has no sockets or timers: observations come from
//! STUN gathering, while the direct path consumes the resulting bounded plan.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NatMappingClass {
    EndpointIndependent,
    AddressDependent,
    PortDependent,
    Symmetric,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NatProfile {
    pub mapping: Option<NatMappingClass>,
    #[serde(default)]
    pub reflexive_addrs: Vec<SocketAddr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatPlanMode {
    Fast,
    Aggressive,
    RelayPreferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NatPlan {
    pub mode: NatPlanMode,
    pub punch_rounds: u8,
    pub check_window_ms: u64,
}

/// Classify a sequence of reflexive mappings gathered through distinct STUN
/// targets. Stable mappings are endpoint-independent; a stable public IP with
/// changing ports is address-dependent; multiple public IPs are symmetric.
pub fn classify_nat(observations: &[SocketAddr]) -> NatMappingClass {
    let Some(first) = observations.first() else {
        return NatMappingClass::Unknown;
    };
    if observations.iter().all(|addr| addr == first) {
        return NatMappingClass::EndpointIndependent;
    }
    if observations.iter().all(|addr| addr.ip() == first.ip()) {
        return NatMappingClass::AddressDependent;
    }
    NatMappingClass::Symmetric
}

/// Build bounded punch/check timing from the two observed NAT profiles.
pub fn plan_for(local: NatMappingClass, peer: NatMappingClass) -> NatPlan {
    use NatMappingClass::*;
    match (local, peer) {
        (Symmetric, _) | (_, Symmetric) | (Unknown, _) | (_, Unknown) => NatPlan {
            mode: NatPlanMode::RelayPreferred,
            punch_rounds: 2,
            check_window_ms: 250,
        },
        (EndpointIndependent, EndpointIndependent) => NatPlan {
            mode: NatPlanMode::Fast,
            punch_rounds: 3,
            check_window_ms: 500,
        },
        _ => NatPlan {
            mode: NatPlanMode::Aggressive,
            punch_rounds: 8,
            check_window_ms: 1_500,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(value: &str) -> SocketAddr {
        value.parse().expect("test socket address")
    }

    #[test]
    fn classification_matrix_is_pure_and_table_driven() {
        assert_eq!(classify_nat(&[]), NatMappingClass::Unknown);
        assert_eq!(
            classify_nat(&[addr("198.51.100.1:5000"), addr("198.51.100.1:5000")]),
            NatMappingClass::EndpointIndependent
        );
        assert_eq!(
            classify_nat(&[addr("198.51.100.1:5000"), addr("198.51.100.1:5001")]),
            NatMappingClass::AddressDependent
        );
        assert_eq!(
            classify_nat(&[addr("198.51.100.1:5000"), addr("203.0.113.1:5001")]),
            NatMappingClass::Symmetric
        );
    }

    #[test]
    fn plan_selection_prefers_relay_only_for_unknown_or_symmetric() {
        assert_eq!(
            plan_for(
                NatMappingClass::EndpointIndependent,
                NatMappingClass::EndpointIndependent
            )
            .mode,
            NatPlanMode::Fast
        );
        assert_eq!(
            plan_for(
                NatMappingClass::AddressDependent,
                NatMappingClass::PortDependent
            )
            .mode,
            NatPlanMode::Aggressive
        );
        assert_eq!(
            plan_for(
                NatMappingClass::Symmetric,
                NatMappingClass::EndpointIndependent
            )
            .mode,
            NatPlanMode::RelayPreferred
        );
    }
}
