#![forbid(unsafe_code)]
//! The `zoologist` application: command-line interface, pipeline wiring and HTTP server.

pub mod analysis;
pub mod api;
pub mod app;
pub mod cli;
pub mod demo;
pub mod disk;
pub mod healthcheck;
pub mod hub_import;
pub mod hub_test;
pub mod janitor;
pub mod live;
pub mod pipeline;
pub mod prune;
pub mod reclassify;
pub mod replay;
pub mod snapshots;
pub mod tools;
pub mod writer;
