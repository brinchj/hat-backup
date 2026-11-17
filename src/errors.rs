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

pub use self::crypto_error::CryptoError;
pub use self::diesel_error::DieselError;

pub use self::hat_error::HatError;
use std::{error, fmt};

#[derive(Clone, Copy, Debug)]
pub struct RetryError;

impl fmt::Display for RetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Retry request")
    }
}

impl error::Error for RetryError {}

mod hat_error {
    use crate::blob;
    use crate::key;
    use std::borrow::Cow;
    use std::sync::mpsc;
    use std::{io, str};
    use void;

    #[derive(Debug, thiserror::Error)]
    pub enum HatError {
        #[error(transparent)]
        Recv(#[from] mpsc::RecvError),
        #[error(transparent)]
        Keys(#[from] key::MsgError),
        #[error(transparent)]
        Serde(#[from] ciborium::de::Error<io::Error>),
        #[error(transparent)]
        SerdeSer(#[from] ciborium::ser::Error<io::Error>),
        #[error(transparent)]
        IO(#[from] io::Error),
        #[error("{0}")]
        Message(Cow<'static, str>),
        #[error(transparent)]
        DieselError(#[from] super::DieselError),
        #[error(transparent)]
        Crypto(#[from] super::CryptoError),
        #[error(transparent)]
        Blob(#[from] blob::BlobError),
    }

    impl From<&'static str> for HatError {
        fn from(s: &'static str) -> Self {
            HatError::Message(Cow::Borrowed(s))
        }
    }

    impl From<String> for HatError {
        fn from(s: String) -> Self {
            HatError::Message(Cow::Owned(s))
        }
    }

    impl From<void::Void> for HatError {
        fn from(val: void::Void) -> HatError {
            void::unreachable(val)
        }
    }

    impl From<str::Utf8Error> for HatError {
        fn from(e: str::Utf8Error) -> Self {
            HatError::Message(Cow::Owned(e.to_string()))
        }
    }
}

mod diesel_error {
    use diesel;
    use diesel_migrations;

    #[derive(Debug, thiserror::Error)]
    pub enum DieselError {
        #[error(transparent)]
        SqlConnection(#[from] diesel::ConnectionError),
        #[error(transparent)]
        SqlMigration(#[from] Box<dyn std::error::Error + Send + Sync>),
        #[error(transparent)]
        SqlExecute(#[from] diesel::result::Error),
    }

    impl From<diesel_migrations::MigrationError> for DieselError {
        fn from(e: diesel_migrations::MigrationError) -> Self {
            DieselError::SqlMigration(Box::new(e))
        }
    }
}

mod crypto_error {
    use std::borrow::Cow;
    use std::str;

    #[derive(Debug, thiserror::Error)]
    pub enum CryptoError {
        #[error("{0}")]
        Message(Cow<'static, str>),
    }

    impl From<&'static str> for CryptoError {
        fn from(s: &'static str) -> Self {
            CryptoError::Message(Cow::Borrowed(s))
        }
    }

    impl From<String> for CryptoError {
        fn from(s: String) -> Self {
            CryptoError::Message(Cow::Owned(s))
        }
    }

    impl From<str::Utf8Error> for CryptoError {
        fn from(e: str::Utf8Error) -> Self {
            CryptoError::Message(Cow::Owned(e.to_string()))
        }
    }
}
