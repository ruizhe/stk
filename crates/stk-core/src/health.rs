use crate::config::LoadBalancePolicy;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Unknown,
    Healthy,
    Degraded,
    Offline,
}

impl HealthStatus {
    pub fn is_selectable(self) -> bool {
        matches!(self, Self::Unknown | Self::Healthy | Self::Degraded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHostState {
    pub name: String,
    pub enabled: bool,
    pub status: HealthStatus,
    pub rtt_millis: Option<u64>,
    pub in_flight: usize,
}

impl SshHostState {
    pub fn selectable(name: impl Into<String>, rtt_millis: Option<u64>) -> Self {
        Self {
            name: name.into(),
            enabled: true,
            status: HealthStatus::Healthy,
            rtt_millis,
            in_flight: 0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LoadBalancer {
    policy: LoadBalancePolicy,
    #[cfg(test)]
    next: AtomicUsize,
}

impl LoadBalancer {
    pub(crate) fn new(policy: LoadBalancePolicy) -> Self {
        Self {
            policy,
            #[cfg(test)]
            next: AtomicUsize::new(0),
        }
    }

    pub fn select<'a>(&self, candidates: &'a [SshHostState]) -> Option<&'a SshHostState> {
        let selectable = candidates
            .iter()
            .filter(|candidate| candidate.enabled && candidate.status.is_selectable())
            .collect::<Vec<_>>();

        if selectable.is_empty() {
            return None;
        }

        match self.policy {
            #[cfg(test)]
            LoadBalancePolicy::RoundRobin => {
                let index = self.next.fetch_add(1, Ordering::Relaxed) % selectable.len();
                Some(selectable[index])
            }
            #[cfg(test)]
            LoadBalancePolicy::LeastLatency => selectable
                .into_iter()
                .min_by_key(|candidate| candidate.rtt_millis.unwrap_or(u64::MAX)),
            LoadBalancePolicy::WeightedRtt => selectable.into_iter().min_by_key(|candidate| {
                candidate
                    .rtt_millis
                    .unwrap_or(u64::MAX / 2)
                    .saturating_mul(candidate.in_flight.saturating_add(1) as u64)
            }),
            #[cfg(test)]
            LoadBalancePolicy::Failover => selectable.into_iter().next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn least_latency_prefers_lowest_rtt() {
        let balancer = LoadBalancer::new(LoadBalancePolicy::LeastLatency);
        let candidates = vec![
            SshHostState::selectable("slow", Some(180)),
            SshHostState::selectable("fast", Some(40)),
        ];

        let selected = balancer.select(&candidates).unwrap();
        assert_eq!(selected.name, "fast");
    }

    #[test]
    fn round_robin_rotates_candidates() {
        let balancer = LoadBalancer::new(LoadBalancePolicy::RoundRobin);
        let candidates = vec![
            SshHostState::selectable("a", Some(10)),
            SshHostState::selectable("b", Some(20)),
        ];

        assert_eq!(balancer.select(&candidates).unwrap().name, "a");
        assert_eq!(balancer.select(&candidates).unwrap().name, "b");
        assert_eq!(balancer.select(&candidates).unwrap().name, "a");
    }

    #[test]
    fn offline_candidates_are_skipped() {
        let balancer = LoadBalancer::new(LoadBalancePolicy::Failover);
        let candidates = vec![
            SshHostState {
                name: "offline".to_string(),
                enabled: true,
                status: HealthStatus::Offline,
                rtt_millis: Some(1),
                in_flight: 0,
            },
            SshHostState::selectable("healthy", Some(100)),
        ];

        assert_eq!(balancer.select(&candidates).unwrap().name, "healthy");
    }
}
