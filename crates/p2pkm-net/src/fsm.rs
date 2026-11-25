use p2pkm_proto::PeerId;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusState {
    Local,
    Controlling {
        target_peer: PeerId,
        epoch: u64,
        last_peer_ack: Instant,
    },
    Controlled {
        controller_peer: PeerId,
        epoch: u64,
        last_activity: Instant,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusDecision {
    Granted { epoch: u64 },
    Rejected { current_controller: PeerId, epoch: u64 },
}

pub struct FocusFsm {
    pub local_peer_id: PeerId,
    pub state: FocusState,
    pub epoch: u64,
    pub lease_timeout: Duration,
}

impl FocusFsm {
    pub fn new(local_peer_id: PeerId, lease_timeout: Duration) -> Self {
        Self {
            local_peer_id,
            state: FocusState::Local,
            epoch: 1,
            lease_timeout,
        }
    }

    /// Called when local user crosses a screen edge toward `target_peer`.
    /// Returns the new epoch to include in `FocusRequest`, or `None` if already busy.
    pub fn begin_outgoing_request(&mut self) -> Option<u64> {
        match self.state {
            FocusState::Local => {
                self.epoch += 1;
                Some(self.epoch)
            }
            _ => None,
        }
    }

    /// Called when `target_peer` replies with `FocusAck`.
    pub fn on_focus_ack(&mut self, target_peer: PeerId, ack_epoch: u64) -> bool {
        if matches!(self.state, FocusState::Local) && ack_epoch == self.epoch {
            self.state = FocusState::Controlling {
                target_peer,
                epoch: ack_epoch,
                last_peer_ack: Instant::now(),
            };
            true
        } else {
            false
        }
    }

    /// Called when another peer sends `FocusRequest` to control this machine.
    pub fn on_incoming_focus_request(
        &mut self,
        from_peer: PeerId,
        req_epoch: u64,
        now: Instant,
    ) -> FocusDecision {
        match &self.state {
            FocusState::Local => {
                self.epoch = self.epoch.max(req_epoch) + 1;
                self.state = FocusState::Controlled {
                    controller_peer: from_peer,
                    epoch: req_epoch,
                    last_activity: now,
                };
                FocusDecision::Granted { epoch: req_epoch }
            }
            FocusState::Controlled {
                controller_peer,
                epoch,
                ..
            } => {
                if *controller_peer == from_peer {
                    self.state = FocusState::Controlled {
                        controller_peer: from_peer,
                        epoch: req_epoch,
                        last_activity: now,
                    };
                    FocusDecision::Granted { epoch: req_epoch }
                } else {
                    FocusDecision::Rejected {
                        current_controller: *controller_peer,
                        epoch: *epoch,
                    }
                }
            }
            FocusState::Controlling {
                target_peer,
                epoch,
                ..
            } => {
                // Simultaneous crossing race condition: deterministic tie-breaker
                let win = req_epoch > *epoch
                    || (req_epoch == *epoch && from_peer < self.local_peer_id);
                if win {
                    self.state = FocusState::Controlled {
                        controller_peer: from_peer,
                        epoch: req_epoch,
                        last_activity: now,
                    };
                    FocusDecision::Granted { epoch: req_epoch }
                } else {
                    FocusDecision::Rejected {
                        current_controller: *target_peer,
                        epoch: *epoch,
                    }
                }
            }
        }
    }

    pub fn refresh_controlled_activity(&mut self, from_peer: PeerId, now: Instant) -> bool {
        if let FocusState::Controlled {
            controller_peer,
            last_activity,
            ..
        } = &mut self.state
        {
            if *controller_peer == from_peer {
                *last_activity = now;
                return true;
            }
        }
        false
    }

    pub fn refresh_controlling_ack(&mut self, from_peer: PeerId, now: Instant) -> bool {
        if let FocusState::Controlling {
            target_peer,
            last_peer_ack,
            ..
        } = &mut self.state
        {
            if *target_peer == from_peer {
                *last_peer_ack = now;
                return true;
            }
        }
        false
    }

    /// Check if `Controlled` or `Controlling` lease has expired (network drop / client crash / sleep).
    /// Returns `true` if timed out and transitioned back to `Local`.
    pub fn check_lease_timeout(&mut self, now: Instant) -> bool {
        match self.state {
            FocusState::Controlled { last_activity, .. } => {
                if now.duration_since(last_activity) >= self.lease_timeout {
                    self.state = FocusState::Local;
                    return true;
                }
            }
            FocusState::Controlling { last_peer_ack, .. } => {
                // Controller reclaims local control if target stops acking heartbeats
                let controller_timeout = self.lease_timeout.max(Duration::from_millis(800));
                if now.duration_since(last_peer_ack) >= controller_timeout {
                    self.state = FocusState::Local;
                    return true;
                }
            }
            FocusState::Local => {}
        }
        false
    }

    /// Local physical preemption or emergency panic hotkey.
    pub fn force_local(&mut self) -> Option<PeerId> {
        let prev = match self.state {
            FocusState::Controlling { target_peer, .. } => Some(target_peer),
            FocusState::Controlled {
                controller_peer, ..
            } => Some(controller_peer),
            FocusState::Local => None,
        };
        self.state = FocusState::Local;
        prev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_focus_transfer_and_revoke() {
        let peer_a = [1u8; 16];
        let peer_b = [2u8; 16];
        let mut fsm_a = FocusFsm::new(peer_a, Duration::from_millis(500));
        let mut fsm_b = FocusFsm::new(peer_b, Duration::from_millis(500));

        let epoch = fsm_a.begin_outgoing_request().unwrap();
        let now = Instant::now();
        assert_eq!(
            fsm_b.on_incoming_focus_request(peer_a, epoch, now),
            FocusDecision::Granted { epoch }
        );
        assert!(fsm_a.on_focus_ack(peer_b, epoch));
        assert!(matches!(fsm_a.state, FocusState::Controlling { .. }));
        assert!(matches!(fsm_b.state, FocusState::Controlled { .. }));

        // Preemption on B returns B to Local
        assert_eq!(fsm_b.force_local(), Some(peer_a));
        assert_eq!(fsm_b.state, FocusState::Local);
    }

    #[test]
    fn simultaneous_race_condition_is_deterministic() {
        let peer_a = [1u8; 16];
        let peer_b = [2u8; 16];
        let peer_c = [3u8; 16];
        let mut fsm_c = FocusFsm::new(peer_c, Duration::from_millis(500));
        let now = Instant::now();

        // A arrives first
        assert_eq!(
            fsm_c.on_incoming_focus_request(peer_a, 5, now),
            FocusDecision::Granted { epoch: 5 }
        );
        // B arrives immediately after -> rejected with current_controller = A
        assert_eq!(
            fsm_c.on_incoming_focus_request(peer_b, 5, now),
            FocusDecision::Rejected {
                current_controller: peer_a,
                epoch: 5
            }
        );
    }

    #[test]
    fn heartbeat_timeout_returns_to_local() {
        let peer_a = [1u8; 16];
        let peer_b = [2u8; 16];
        let mut fsm_b = FocusFsm::new(peer_b, Duration::from_millis(500));
        let t0 = Instant::now();

        fsm_b.on_incoming_focus_request(peer_a, 2, t0);
        assert!(!fsm_b.check_lease_timeout(t0 + Duration::from_millis(300)));
        assert!(fsm_b.check_lease_timeout(t0 + Duration::from_millis(501)));
        assert_eq!(fsm_b.state, FocusState::Local);
    }
}
