#![forbid(unsafe_code)]
//! Camera ingest, H.264 decoding, MP4 recording and clip building.

pub mod capture;
pub mod clips;
pub mod decode;
pub mod file_source;
pub mod flv;
pub mod h264;
pub mod http;
pub mod mp4r;
pub mod mp4w;
pub mod onvif;
pub mod recorder;
pub mod reolink_hub;
pub mod snapshot;
pub mod source;
pub mod stream;
