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

use diesel::sql_types::BigInt;

// Table schemas.

table! {
    hashes {
        id -> BigInt,
        hash -> Binary,
        tag -> BigInt,
        height -> BigInt,
        leaf_type -> BigInt,
        childs -> Nullable<Binary>,
        blob_id -> BigInt,
        blob_ref -> Nullable<Binary>,
        ready -> Bool,
    }
}

table! {
    blobs {
        id -> BigInt,
        name -> Binary,
        tag -> Integer,
    }
}

table! {
    family {
        id -> BigInt,
        name -> VarChar,
    }
}

table! {
    snapshots {
        id -> BigInt,
        tag -> Integer,
        family_id -> BigInt,
        snapshot_id -> BigInt,
        utc_datetime -> Timestamp,
        msg -> Nullable<VarChar>,
        hash -> Nullable<Binary>,
        hash_ref -> Nullable<Binary>,
    }
}

table! {
    stat_cache (family_id, rel_path) {
        family_id -> BigInt,
        rel_path -> Binary,
        kind -> Integer,
        created -> Nullable<BigInt>,
        modified -> Nullable<BigInt>,
        accessed -> Nullable<BigInt>,
        permissions -> Nullable<BigInt>,
        user_id -> Nullable<BigInt>,
        group_id -> Nullable<BigInt>,
        file_size -> Nullable<BigInt>,
        dev -> Nullable<BigInt>,
        ino -> Nullable<BigInt>,
        ctime -> Nullable<BigInt>,
        ctime_nsec -> Nullable<BigInt>,
        mtime_nsec -> Nullable<BigInt>,
        snapshot_ts_utc -> BigInt,
        hash -> Nullable<Binary>,
        hash_ref -> Nullable<Binary>,
        symlink_target -> Nullable<Binary>,
    }
}

table! {
    tree_edges (tree_hash_id, child_hash_id) {
        tree_hash_id -> BigInt,
        child_hash_id -> BigInt,
    }
}

joinable!(snapshots -> family (family_id));
joinable!(hashes -> blobs (blob_id));

allow_tables_to_appear_in_same_query!(blobs, family, hashes, snapshots, stat_cache, tree_edges,);

// Rust models.

#[derive(Queryable, QueryableByName)]
pub struct RowId {
    #[diesel(sql_type = BigInt)]
    pub row_id: i64,
}

#[derive(Queryable)]
#[allow(dead_code)]
pub struct Hash {
    pub id: i64,
    pub hash: Vec<u8>,
    pub tag: i64,
    pub height: i64,
    pub leaf_type: i64,
    pub childs: Option<Vec<u8>>,
    pub blob_id: i64,
    pub blob_ref: Option<Vec<u8>>,
    pub ready: bool,
}

#[derive(Insertable)]
#[diesel(table_name = hashes)]
pub struct NewHash<'a> {
    pub id: i64,
    pub hash: &'a [u8],
    pub tag: i64,
    pub height: i64,
    pub leaf_type: i64,
    pub childs: Option<&'a [u8]>,
    pub blob_id: i64,
    pub blob_ref: Option<&'a [u8]>,
    pub ready: bool,
}

#[derive(Queryable)]
#[allow(dead_code)]
pub struct Blob {
    pub id: i64,
    pub name: Vec<u8>,
    pub tag: i32,
}

#[derive(Insertable)]
#[diesel(table_name = blobs)]
pub struct NewBlob<'a> {
    pub id: i64,
    pub name: &'a [u8],
    pub tag: i32,
}

#[derive(Queryable)]
pub struct Family {
    pub id: i64,
    pub name: String,
}

#[derive(Insertable)]
#[diesel(table_name = family)]
pub struct NewFamily<'a> {
    pub name: &'a str,
}

#[derive(Queryable)]
pub struct Snapshot {
    pub id: i64,
    pub tag: i32,
    pub family_id: i64,
    pub snapshot_id: i64,
    pub utc_datetime: chrono::NaiveDateTime,
    pub msg: Option<String>,
    pub hash: Option<Vec<u8>>,
    pub hash_ref: Option<Vec<u8>>,
}

#[derive(Insertable)]
#[diesel(table_name = snapshots)]
pub struct NewSnapshot<'a> {
    pub tag: i32,
    pub family_id: i64,
    pub snapshot_id: i64,
    pub utc_datetime: chrono::NaiveDateTime,
    pub msg: Option<&'a str>,
    pub hash: Option<&'a [u8]>,
    pub hash_ref: Option<&'a [u8]>,
}

#[derive(Queryable)]
#[allow(dead_code)]
pub struct StatCacheRow {
    pub family_id: i64,
    pub rel_path: Vec<u8>,
    pub kind: i32,
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub accessed: Option<i64>,
    pub permissions: Option<i64>,
    pub user_id: Option<i64>,
    pub group_id: Option<i64>,
    pub file_size: Option<i64>,
    pub dev: Option<i64>,
    pub ino: Option<i64>,
    pub ctime: Option<i64>,
    pub ctime_nsec: Option<i64>,
    pub mtime_nsec: Option<i64>,
    pub snapshot_ts_utc: i64,
    pub hash: Option<Vec<u8>>,
    pub hash_ref: Option<Vec<u8>>,
    pub symlink_target: Option<Vec<u8>>,
}

#[derive(Insertable)]
#[diesel(table_name = stat_cache)]
pub struct NewStatCacheRow<'a> {
    pub family_id: i64,
    pub rel_path: &'a [u8],
    pub kind: i32,
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub accessed: Option<i64>,
    pub permissions: Option<i64>,
    pub user_id: Option<i64>,
    pub group_id: Option<i64>,
    pub file_size: Option<i64>,
    pub dev: Option<i64>,
    pub ino: Option<i64>,
    pub ctime: Option<i64>,
    pub ctime_nsec: Option<i64>,
    pub mtime_nsec: Option<i64>,
    pub snapshot_ts_utc: i64,
    pub hash: Option<&'a [u8]>,
    pub hash_ref: Option<&'a [u8]>,
    pub symlink_target: Option<&'a [u8]>,
}

#[derive(Insertable)]
#[diesel(table_name = tree_edges)]
pub struct NewTreeEdge {
    pub tree_hash_id: i64,
    pub child_hash_id: i64,
}
