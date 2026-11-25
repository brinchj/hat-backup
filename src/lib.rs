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

#![forbid(unsafe_code)]

// Unstable APIs:
#![cfg_attr(feature = "benchmarks", feature(test))]

#[cfg(feature = "benchmarks")]
extern crate test;

#[macro_use]
extern crate diesel;
#[macro_use]
extern crate diesel_migrations;
#[macro_use]
extern crate log;

// Submodules
pub mod backend;
mod cbor_util;
mod blob;
pub mod crypto;
mod db;
mod errors;
mod hash;
pub mod hat;
mod key;
pub mod models;
mod snapshot;
mod tags;
pub mod util;
pub mod vfs;

// Re-export the main type

pub use crate::hat::Hat;
