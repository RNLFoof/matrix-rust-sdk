// Copyright 2024 The Matrix.org Foundation C.I.C.
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

#![allow(missing_docs)]

use matrix_sdk_base::crypto::SecretImportError;
use thiserror::Error;
use vodozemac::secure_channel::SecureChannelError as EciesError;

use crate::HttpError;

mod grant_login;
mod login;
mod messages;
mod rendezvous_channel;
mod requests;
mod secure_channel;

pub use grant_login::ExistingAuthGrantDings;
pub use login::{LoginProgress, LoginWithQrCode};
pub use matrix_sdk_base::crypto::qr_login::QrCodeData;

use self::messages::QrAuthMessage;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    UrlParse(#[from] url::ParseError),
    #[error(transparent)]
    Ecies(#[from] EciesError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    SecretImport(#[from] SecretImportError),
    #[error("We have received an unexpected message, expected: {expected}, got {received:?}.")]
    UnexpectedMessage { expected: &'static str, received: QrAuthMessage },
}
