/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::sync::Arc;

use utils::config::{utils::AsKey, Config};

use crate::{Directory, DirectoryOptions, Principal, Type};

use super::{EmailType, MemoryDirectory};

impl MemoryDirectory {
    pub fn from_config(
        config: &Config,
        prefix: impl AsKey,
    ) -> utils::config::Result<Arc<dyn Directory>> {
        let prefix = prefix.as_key();
        let mut directory = MemoryDirectory {
            opt: DirectoryOptions::from_config(config, prefix.clone())?,
            ..Default::default()
        };

        for lookup_id in config.sub_keys((prefix.as_str(), "principals")) {
            let name = config
                .value_require((prefix.as_str(), "principals", lookup_id, "name"))?
                .to_string();
            let typ =
                match config.value_require((prefix.as_str(), "principals", lookup_id, "name"))? {
                    "individual" => Type::Individual,
                    "admin" => Type::Superuser,
                    "group" => Type::Group,
                    _ => Type::Other,
                };

            // Obtain id
            let next_user_id = directory.names_to_ids.len() as u32;
            let id = *directory
                .names_to_ids
                .entry(name.to_string())
                .or_insert(next_user_id);

            // Obtain group ids
            let mut member_of = Vec::new();
            for (_, group) in config.values((prefix.as_str(), "principals", lookup_id, "member-of"))
            {
                let next_group_id = directory.names_to_ids.len() as u32;
                member_of.push(
                    *directory
                        .names_to_ids
                        .entry(group.to_string())
                        .or_insert(next_group_id),
                );
            }

            // Parse email addresses
            let mut emails = Vec::new();
            for (pos, (_, email)) in config
                .values((prefix.as_str(), "principals", lookup_id, "email"))
                .enumerate()
            {
                directory
                    .emails_to_ids
                    .entry(email.to_string())
                    .or_default()
                    .push(if pos > 0 {
                        EmailType::Alias(id)
                    } else {
                        EmailType::Primary(id)
                    });

                if let Some((_, domain)) = email.rsplit_once('@') {
                    directory.domains.insert(domain.to_lowercase());
                }

                emails.push(email.to_lowercase());
            }

            // Parse mailing lists
            for (_, email) in
                config.values((prefix.as_str(), "principals", lookup_id, "email-list"))
            {
                directory
                    .emails_to_ids
                    .entry(email.to_lowercase())
                    .or_default()
                    .push(EmailType::List(id));
                if let Some((_, domain)) = email.rsplit_once('@') {
                    directory.domains.insert(domain.to_lowercase());
                }
            }

            directory.principals.push(Principal {
                name: name.clone(),
                secrets: config
                    .values((prefix.as_str(), "principals", lookup_id, "secret"))
                    .map(|(_, v)| v.to_string())
                    .collect(),
                typ,
                description: config
                    .value((prefix.as_str(), "principals", lookup_id, "description"))
                    .map(|v| v.to_string()),
                quota: config
                    .property((prefix.as_str(), "principals", lookup_id, "quota"))?
                    .unwrap_or(0),
                member_of,
                id,
                emails,
            });
        }

        Ok(Arc::new(directory))
    }
}
