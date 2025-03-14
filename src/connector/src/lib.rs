// Copyright 2023 RisingWave Labs
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

#![expect(dead_code)]
#![allow(clippy::derive_partial_eq_without_eq)]
#![feature(generators)]
#![feature(proc_macro_hygiene)]
#![feature(stmt_expr_attributes)]
#![feature(box_patterns)]
#![feature(trait_alias)]
#![feature(binary_heap_drain_sorted)]
#![feature(lint_reasons)]
#![feature(once_cell)]
#![feature(result_option_inspect)]
#![feature(let_chains)]
#![feature(box_into_inner)]
#![feature(type_alias_impl_trait)]

use std::time::Duration;

use duration_str::parse_std;
use serde::de;

pub mod aws_utils;
pub mod error;
mod macros;

pub mod parser;
pub mod sink;
pub mod source;

pub mod common;

#[derive(Clone, Debug, Default)]
pub struct ConnectorParams {
    pub connector_rpc_endpoint: Option<String>,
}

impl ConnectorParams {
    pub fn new(connector_rpc_endpoint: Option<String>) -> Self {
        Self {
            connector_rpc_endpoint,
        }
    }
}

pub(crate) fn deserialize_bool_from_string<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: de::Deserializer<'de>,
{
    let s: String = de::Deserialize::deserialize(deserializer)?;
    let s = s.to_ascii_lowercase();
    match s.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(de::Error::invalid_value(
            de::Unexpected::Str(&s),
            &"true or false",
        )),
    }
}

pub(crate) fn deserialize_duration_from_string<'de, D>(
    deserializer: D,
) -> Result<Duration, D::Error>
where
    D: de::Deserializer<'de>,
{
    let s: String = de::Deserialize::deserialize(deserializer)?;
    parse_std(&s).map_err(|_| de::Error::invalid_value(
        de::Unexpected::Str(&s),
        &"The String value unit support for one of:[“y”,“mon”,“w”,“d”,“h”,“m”,“s”, “ms”, “µs”, “ns”]",
    ))
}
