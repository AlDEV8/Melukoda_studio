use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SafetyPreset {
    Direct,
    Normal,
    Road,
    Safe,
    Extreme,
    Custom(u8),
}
impl SafetyPreset {
    pub fn seconds(self) -> u8 {
        match self {
            Self::Direct => 0,
            Self::Normal => 5,
            Self::Road => 15,
            Self::Safe => 30,
            Self::Extreme => 60,
            Self::Custom(v) => v.min(120),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BroadcastState {
    Ready,
    Connecting,
    Live,
    Buffering,
    Reconnecting,
    CatchingUp,
    BufferExhausted,
    Error,
    Stopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Start,
    Connected,
    Disconnected,
    DelayFilled,
    ReplayStarted,
    ReplayComplete,
    Exhausted,
    Stop,
    Failure,
}
pub fn transition(current: BroadcastState, event: Event) -> BroadcastState {
    use BroadcastState::*;
    use Event::*;
    match (current, event) {
        (_, Stop) => Stopped,
        (Ready, Start) | (Stopped, Start) => Connecting,
        (Connecting, Connected) => Buffering,
        (Buffering, DelayFilled) => Live,
        (Live, Disconnected) | (Buffering, Disconnected) => Reconnecting,
        (Reconnecting, Connected) => CatchingUp,
        (CatchingUp, ReplayComplete) => Live,
        (Reconnecting, Exhausted) => BufferExhausted,
        (_, Failure) => Error,
        (BufferExhausted, Connected) => CatchingUp,
        (s, _) => s,
    }
}
pub fn reconnect_delay_ms(attempt: u32, jitter_ms: u16) -> u64 {
    (500_u64.saturating_mul(2_u64.saturating_pow(attempt.min(7)))).min(30_000)
        + u64::from(jitter_ms % 251)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_machine() {
        let s = transition(BroadcastState::Ready, Event::Start);
        assert_eq!(transition(s, Event::Connected), BroadcastState::Buffering);
        assert_eq!(
            transition(BroadcastState::Reconnecting, Event::Exhausted),
            BroadcastState::BufferExhausted
        );
    }
    #[test]
    fn reconnect_is_bounded() {
        assert!(reconnect_delay_ms(30, 0) <= 30_000);
    }
}
