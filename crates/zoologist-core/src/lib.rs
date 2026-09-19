#![forbid(unsafe_code)]
//! Configuration, domain types and small helpers shared by every Zoologist crate.
//!
//! Nothing here does I/O except [`Config::load`].

pub mod config;
mod error;
mod redact;
mod time;
mod types;
pub mod yuv;

pub use config::Config;
pub use error::ConfigError;
pub use redact::redact_url;
pub use time::{local_date_hour, rfc3339_micros};
pub use types::{BBox, CameraId, Detection, Frame, Label, SpeciesGuess};
