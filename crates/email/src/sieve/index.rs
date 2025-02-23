/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use jmap_proto::{
    object::index::{IndexValue, IndexableObject},
    types::property::Property,
};

use super::SieveScript;

impl IndexableObject for SieveScript {
    fn index_values(&self) -> impl Iterator<Item = IndexValue<'_>> {
        [
            IndexValue::Text {
                field: Property::Name.into(),
                value: self.name.as_str(),
                tokenize: true,
                index: true,
            },
            IndexValue::U32 {
                field: Property::IsActive.into(),
                value: Some(self.is_active as u32),
            },
            IndexValue::Quota {
                used: self.blob_id.section.as_ref().map_or(0, |b| b.size as u32),
            },
        ]
        .into_iter()
    }
}
