/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

pub mod serialize;

use jmap_proto::types::type_state::DataType;
use utils::map::bitmap::Bitmap;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PushSubscription {
    pub url: String,
    pub device_client_id: String,
    pub expires: u64,
    pub verification_code: String,
    pub verified: bool,
    pub types: Bitmap<DataType>,
    pub keys: Option<Keys>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Keys {
    pub p256dh: Vec<u8>,
    pub auth: Vec<u8>,
}
