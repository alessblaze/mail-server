/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use ahash::AHashMap;
use jmap_proto::types::{collection::Collection, property::Property};
use store::{
    Deserialize, IndexKey, IterateParams, SerializeInfallible, U32_LEN, ValueKey,
    write::{Archive, ValueClass, key::DeserializeBigEndian},
};
use trc::AddContext;
use utils::topological::{TopologicalSort, TopologicalSortIterator};

use crate::Server;

pub struct ExpandedFolders {
    names: AHashMap<u32, (String, u32)>,
    iter: TopologicalSortIterator<u32>,
}

pub trait FolderHierarchy: Sync + Send {
    fn name(&self) -> String;
    fn parent_id(&self) -> u32;
}

pub trait TopologyBuilder: Sync + Send {
    fn insert(&mut self, folder_id: u32, parent_id: u32);
}

impl Server {
    pub async fn fetch_folders<T>(
        &self,
        account_id: u32,
        collection: Collection,
    ) -> trc::Result<ExpandedFolders>
    where
        T: rkyv::Archive,
        T::Archived: FolderHierarchy
            + for<'a> rkyv::bytecheck::CheckBytes<
                rkyv::api::high::HighValidator<'a, rkyv::rancor::Error>,
            > + rkyv::Deserialize<T, rkyv::api::high::HighDeserializer<rkyv::rancor::Error>>,
    {
        let collection_: u8 = collection.into();

        let mut names = AHashMap::with_capacity(10);
        let mut topological_sort = TopologicalSort::with_capacity(10);

        self.core
            .storage
            .data
            .iterate(
                IterateParams::new(
                    ValueKey {
                        account_id,
                        collection: collection_,
                        document_id: 0,
                        class: ValueClass::Property(Property::Value.into()),
                    },
                    ValueKey {
                        account_id,
                        collection: collection_,
                        document_id: u32::MAX,
                        class: ValueClass::Property(Property::Value.into()),
                    },
                ),
                |key, value| {
                    let document_id = key.deserialize_be_u32(key.len() - U32_LEN)? + 1;
                    let archive = <Archive as Deserialize>::deserialize(value)?;
                    let folder = archive.unarchive::<T>()?;
                    let parent_id = folder.parent_id();

                    topological_sort.insert(parent_id, document_id);
                    names.insert(document_id, (folder.name(), parent_id));

                    Ok(true)
                },
            )
            .await
            .add_context(|err| {
                err.caused_by(trc::location!())
                    .account_id(account_id)
                    .collection(collection)
            })?;

        Ok(ExpandedFolders {
            names,
            iter: topological_sort.into_iterator(),
        })
    }

    pub async fn fetch_folder_topology<T>(
        &self,
        account_id: u32,
        collection: Collection,
        topology: &mut impl TopologyBuilder,
    ) -> trc::Result<()>
    where
        T: TopologyBuilder,
    {
        self.store()
            .iterate(
                IterateParams::new(
                    IndexKey {
                        account_id,
                        collection: collection.into(),
                        document_id: 0,
                        field: Property::ParentId.into(),
                        key: 0u32.serialize(),
                    },
                    IndexKey {
                        account_id,
                        collection: collection.into(),
                        document_id: u32::MAX,
                        field: Property::ParentId.into(),
                        key: u32::MAX.serialize(),
                    },
                )
                .no_values()
                .ascending(),
                |key, _| {
                    let document_id = key
                        .get(key.len() - U32_LEN..)
                        .ok_or_else(|| trc::Error::corrupted_key(key, None, trc::location!()))
                        .and_then(u32::deserialize)?;
                    let parent_id = key
                        .get(key.len() - (U32_LEN * 2)..key.len() - U32_LEN)
                        .ok_or_else(|| trc::Error::corrupted_key(key, None, trc::location!()))
                        .and_then(u32::deserialize)?;

                    topology.insert(document_id, parent_id);

                    Ok(true)
                },
            )
            .await
            .caused_by(trc::location!())?;

        Ok(())
    }
}

impl ExpandedFolders {
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn format<T>(mut self, formatter: T) -> Self
    where
        T: Fn(u32, &str) -> Option<String>,
    {
        for (document_id, (name, _)) in &mut self.names {
            if let Some(new_name) = formatter(*document_id - 1, name) {
                *name = new_name;
            }
        }
        self
    }

    pub fn into_iterator(mut self) -> impl Iterator<Item = (u32, String)> + Sync + Send {
        for folder_id in self.iter.by_ref() {
            if folder_id != 0 {
                if let Some((name, parent_name, parent_id)) =
                    self.names.get(&folder_id).and_then(|(name, parent_id)| {
                        self.names
                            .get(parent_id)
                            .map(|(parent_name, _)| (name, parent_name, *parent_id))
                    })
                {
                    let name = format!("{parent_name}/{name}");
                    self.names.insert(folder_id, (name, parent_id));
                }
            }
        }

        self.names.into_iter().map(|(id, (name, _))| (id - 1, name))
    }
}
