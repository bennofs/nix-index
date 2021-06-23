#![cfg_attr(
    feature = "cargo-clippy",
    warn(
        filter_map,
        option_map_unwrap_or,
        option_map_unwrap_or_else,
        option_unwrap_used,
        stutter,
        wrong_pub_self_convention,
        print_stdout
    )
)]

#[macro_use]
extern crate serde_derive;
extern crate ansi_term;
extern crate bincode;
extern crate byteorder;
extern crate futures;
extern crate grep;
extern crate hyper;
extern crate hyper_proxy;
extern crate memchr;
extern crate ordermap;
extern crate regex;
extern crate regex_syntax;
extern crate serde;
extern crate serde_bytes;
extern crate serde_json;
extern crate tokio_core;
extern crate tokio_retry;
extern crate tokio_timer;
extern crate void;
extern crate xml;
extern crate xz2;
extern crate zstd;
#[macro_use]
extern crate thiserror;
extern crate brotli2;
extern crate url;

pub mod database;
pub mod files;
pub mod frcode;
pub mod hydra;
pub mod nixpkgs;
pub mod package;
pub mod util;
pub mod workset;
