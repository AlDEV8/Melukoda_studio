use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    SrtContribution,
    Icecast,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretRef {
    pub service: String,
    pub account: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub mode: OutputMode,
    pub host: String,
    pub port: u16,
    pub stream_id: Option<String>,
    pub mount: Option<String>,
    pub username: Option<String>,
    pub secret: Option<SecretRef>,
    #[serde(default)]
    pub credential_mode: Option<OutputMode>,
    pub tls: bool,
    pub bitrate_kbps: u16,
    pub channels: u8,
    pub program_name: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("profile name is required")]
    Name,
    #[error("host is required")]
    Host,
    #[error("port must be 1–65535")]
    Port,
    #[error("SRT requires a stream ID")]
    StreamId,
    #[error("Icecast requires a mount beginning with /")]
    Mount,
    #[error("AAC-LC bitrate must be 32–320 kbps")]
    Bitrate,
    #[error("only mono or stereo output is supported")]
    Channels,
}

impl Profile {
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.name.trim().is_empty() {
            return Err(ProfileError::Name);
        };
        if self.host.trim().is_empty() || self.host.contains(['/', ' ', '@']) {
            return Err(ProfileError::Host);
        };
        if self.port == 0 {
            return Err(ProfileError::Port);
        };
        if !(32..=320).contains(&self.bitrate_kbps) {
            return Err(ProfileError::Bitrate);
        };
        if ![1, 2].contains(&self.channels) {
            return Err(ProfileError::Channels);
        };
        match self.mode {
            OutputMode::SrtContribution if self.stream_id.as_deref().unwrap_or("").is_empty() => {
                Err(ProfileError::StreamId)
            }
            OutputMode::Icecast if !self.mount.as_deref().unwrap_or("").starts_with('/') => {
                Err(ProfileError::Mount)
            }
            _ => Ok(()),
        }
    }
    pub fn export_without_secrets(&self) -> Self {
        let mut safe = self.clone();
        safe.secret = None;
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn srt() -> Profile {
        Profile {
            id: "x".into(),
            name: "road".into(),
            mode: OutputMode::SrtContribution,
            host: "ingest.example".into(),
            port: 9000,
            stream_id: Some("festival".into()),
            mount: None,
            username: None,
            secret: Some(SecretRef {
                service: "m".into(),
                account: "x".into(),
            }),
            credential_mode: Some(OutputMode::SrtContribution),
            tls: true,
            bitrate_kbps: 128,
            channels: 2,
            program_name: None,
        }
    }
    #[test]
    fn validates_srt() {
        assert!(srt().validate().is_ok());
    }
    #[test]
    fn secret_is_not_exported() {
        assert!(srt().export_without_secrets().secret.is_none());
    }
}
