// Copyright 2014 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! High level Hat API

// Unstable APIs: Bencher
#![cfg_attr(feature = "benchmarks", feature(test))]

// Unstable Plugins: Flamer flamegraphs for profiling.
#![cfg_attr(feature="flame_it", feature(plugin, custom_attribute))]
#![cfg_attr(feature="flame_it", plugin(flamer))]

// Standard Rust imports.
#[macro_use]
extern crate log;
extern crate rand;
#[cfg(feature = "benchmarks")]
extern crate test;
extern crate time;

// Rust crates.
extern crate argon2rs;
extern crate byteorder;
extern crate chrono;
extern crate filetime;
extern crate hex;
extern crate libsodium_sys;
extern crate scoped_pool;
extern crate secstr;
extern crate void;

// Error definition macros.
#[macro_use]
extern crate error_type;

// Diesel supplies our SQLite wrapper.
#[macro_use]
extern crate diesel;
#[macro_use]
extern crate diesel_migrations;

// Testing utilities.
#[cfg(test)]
extern crate quickcheck;

// Serde
extern crate serde;
extern crate serde_cbor;
#[macro_use]
extern crate serde_derive;

// Flamer flamegraphs for profiling.
#[cfg(feature="flame_it")]
pub extern crate flame;

// Submodules
pub mod backend;
pub mod models;
mod blob;
mod crypto;
mod db;
mod errors;
mod gc;
mod hash;
pub mod hat;
mod key;
mod snapshot;
mod tags;
mod util;

// Re-export the main type

pub use hat::Hat;
