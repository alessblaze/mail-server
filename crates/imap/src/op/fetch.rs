/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::{borrow::Cow, sync::Arc, time::Instant};

use crate::{
    core::{SelectedMailbox, Session, SessionData},
    spawn_op,
};
use ahash::AHashMap;
use common::listener::SessionStream;
use directory::Permission;
use email::message::metadata::{
    ArchivedAddress, ArchivedGetHeader, ArchivedHeaderName, ArchivedHeaderValue,
    ArchivedMessageMetadata, ArchivedMessageMetadataContents, ArchivedMetadataPartType,
    DecodedParts,
};
use imap_proto::{
    Command, ResponseCode, ResponseType, StatusResponse,
    parser::PushUnique,
    protocol::{
        Flag,
        expunge::Vanished,
        fetch::{
            self, Arguments, Attribute, BodyContents, BodyPart, BodyPartExtension, BodyPartFields,
            DataItem, Envelope, FetchItem, Section,
        },
    },
    receiver::Request,
};
use jmap_proto::types::{
    acl::Acl,
    collection::Collection,
    id::Id,
    keyword::{ArchivedKeyword, Keyword},
    property::Property,
    state::StateChange,
    type_state::DataType,
};
use mail_parser::DateTime;
use store::{
    Serialize, SerializeInfallible,
    query::log::{Change, Query},
    rkyv::{rend::u16_le, vec::ArchivedVec},
    write::{Archive, Archiver, BatchBuilder, assert::HashedValue, serialize::rkyv_deserialize},
};

use super::{FromModSeq, ImapContext};

impl<T: SessionStream> Session<T> {
    pub async fn handle_fetch(&mut self, requests: Vec<Request<Command>>) -> trc::Result<()> {
        // Validate access
        self.assert_has_permission(Permission::ImapFetch)?;

        let (data, mailbox) = self.state.select_data();
        let is_qresync = self.is_qresync;
        let is_rev2 = self.version.is_rev2();

        let mut ops = Vec::with_capacity(requests.len());

        for request in requests {
            let is_uid = matches!(request.command, Command::Fetch(true));
            match request.parse_fetch() {
                Ok(arguments) => {
                    let enabled_condstore = if !self.is_condstore
                        && arguments.changed_since.is_some()
                        || arguments.attributes.contains(&Attribute::ModSeq)
                    {
                        self.is_condstore = true;
                        true
                    } else {
                        false
                    };

                    ops.push(Ok((is_uid, enabled_condstore, arguments)));
                }
                Err(err) => {
                    ops.push(Err(err));
                }
            }
        }

        spawn_op!(data, {
            for op in ops {
                match op {
                    Ok((is_uid, enabled_condstore, arguments)) => {
                        let response = data
                            .fetch(
                                arguments,
                                mailbox.clone(),
                                is_uid,
                                is_qresync,
                                is_rev2,
                                enabled_condstore,
                                Instant::now(),
                            )
                            .await?;

                        data.write_bytes(response.into_bytes()).await?;
                    }
                    Err(err) => data.write_error(err).await?,
                }
            }

            Ok(())
        })
    }
}

impl<T: SessionStream> SessionData<T> {
    #[allow(clippy::too_many_arguments)]
    pub async fn fetch(
        &self,
        mut arguments: Arguments,
        mailbox: Arc<SelectedMailbox>,
        is_uid: bool,
        is_qresync: bool,
        _is_rev2: bool,
        enabled_condstore: bool,
        op_start: Instant,
    ) -> trc::Result<StatusResponse> {
        // Validate VANISHED parameter
        if arguments.include_vanished {
            if !is_qresync {
                return Err(trc::ImapEvent::Error
                    .into_err()
                    .details("Enable QRESYNC first to use the VANISHED parameter.")
                    .ctx(trc::Key::Type, ResponseType::Bad)
                    .id(arguments.tag));
            } else if !is_uid {
                return Err(trc::ImapEvent::Error
                    .into_err()
                    .details("VANISHED parameter is only available for UID FETCH.")
                    .ctx(trc::Key::Type, ResponseType::Bad)
                    .id(arguments.tag));
            }
        }

        // Resync messages if needed
        let account_id = mailbox.id.account_id;
        let mut modseq = self
            .synchronize_messages(&mailbox)
            .await
            .imap_ctx(&arguments.tag, trc::location!())?;

        // Convert IMAP ids to JMAP ids.
        let mut ids = mailbox
            .sequence_to_ids(&arguments.sequence_set, is_uid)
            .await
            .imap_ctx(&arguments.tag, trc::location!())?;

        // Convert state to modseq
        if let Some(changed_since) = arguments.changed_since {
            // Obtain changes since the modseq.
            let changelog = self
                .server
                .store()
                .changes(
                    account_id,
                    Collection::Email,
                    Query::from_modseq(changed_since),
                )
                .await
                .imap_ctx(&arguments.tag, trc::location!())?;

            // Process changes
            let mut changed_ids = AHashMap::new();
            let mut has_vanished = false;

            for change in changelog.changes {
                match change {
                    Change::Insert(id) | Change::Update(id) | Change::ChildUpdate(id) => {
                        let id = (id & u32::MAX as u64) as u32;
                        if let Some(uid) = ids.get(&id) {
                            changed_ids.insert(id, *uid);
                        }
                        if !has_vanished {
                            has_vanished = matches!(change, Change::Update(_));
                        }
                    }
                    Change::Delete(_) => {
                        has_vanished = true;
                    }
                }
            }

            // Send vanished UIDs
            if arguments.include_vanished && has_vanished {
                // Add to vanished all known destroyed Ids
                let vanished = mailbox
                    .sequence_expand_missing(&arguments.sequence_set, true)
                    .await;

                if !vanished.is_empty() {
                    let mut buf = Vec::with_capacity(vanished.len() * 3);
                    Vanished {
                        earlier: true,
                        ids: vanished,
                    }
                    .serialize(&mut buf);
                    self.write_bytes(buf).await?;
                }
            }

            // Filter out ids without changes
            if changed_ids.is_empty() {
                // Condstore was just enabled, return highest modseq.
                if enabled_condstore {
                    self.write_bytes(
                        StatusResponse::ok("Highest Modseq")
                            .with_code(ResponseCode::highest_modseq(modseq))
                            .into_bytes(),
                    )
                    .await?;
                }

                trc::event!(
                    Imap(trc::ImapEvent::Fetch),
                    SpanId = self.session_id,
                    AccountId = account_id,
                    MailboxId = mailbox.id.mailbox_id,
                    Elapsed = op_start.elapsed()
                );

                return Ok(
                    StatusResponse::completed(Command::Fetch(is_uid)).with_tag(arguments.tag)
                );
            }
            ids = changed_ids;
            arguments.attributes.push_unique(Attribute::ModSeq);
        }

        // Build properties list
        let mut set_seen_flags = false;
        let mut needs_thread_id = false;
        let mut needs_blobs = false;

        for attribute in &arguments.attributes {
            match attribute {
                Attribute::BodySection { sections, .. }
                    if sections.first().is_some_and(|s| {
                        matches!(s, Section::Header | Section::HeaderFields { .. })
                    }) => {}
                Attribute::Body | Attribute::BodyStructure | Attribute::BinarySize { .. } => {
                    /*
                        Note that this did not result in \Seen being set, because
                        RFC822.HEADER response data occurs as a result of a FETCH
                        of RFC822.HEADER.  BODY[HEADER] response data occurs as a
                        result of a FETCH of BODY[HEADER] (which sets \Seen) or
                        BODY.PEEK[HEADER] (which does not set \Seen).
                    */
                    needs_blobs = true;
                }
                Attribute::BodySection { peek, .. } | Attribute::Binary { peek, .. } => {
                    if mailbox.is_select && !*peek {
                        set_seen_flags = true;
                    }
                    needs_blobs = true;
                }
                Attribute::Rfc822Text | Attribute::Rfc822 => {
                    if mailbox.is_select {
                        set_seen_flags = true;
                    }
                    needs_blobs = true;
                }
                Attribute::ThreadId => {
                    needs_thread_id = true;
                }
                _ => (),
            }
        }

        if set_seen_flags
            && !self
                .check_mailbox_acl(
                    mailbox.id.account_id,
                    mailbox.id.mailbox_id,
                    Acl::ModifyItems,
                )
                .await
                .imap_ctx(&arguments.tag, trc::location!())?
        {
            set_seen_flags = false;
        }

        if is_uid {
            if arguments.attributes.is_empty() {
                arguments.attributes.push(Attribute::Flags);
            } else if !arguments.attributes.contains(&Attribute::Uid) {
                arguments.attributes.insert(0, Attribute::Uid);
            }
        }

        let mut set_seen_ids = Vec::new();

        // Process each message
        let mut ids = ids
            .into_iter()
            .map(|(id, imap_id)| (imap_id.seqnum, imap_id.uid, id))
            .collect::<Vec<_>>();
        ids.sort_unstable_by_key(|(seqnum, _, _)| *seqnum);
        let fetched_ids = ids
            .iter()
            .map(|id| trc::Value::from(id.2))
            .collect::<Vec<_>>();

        for (seqnum, uid, id) in ids {
            // Obtain attributes and keywords
            let (email_, keywords_) = if let (Some(email), Some(keywords)) = (
                self.server
                    .get_property::<Archive>(
                        account_id,
                        Collection::Email,
                        id,
                        &Property::BodyStructure,
                    )
                    .await
                    .imap_ctx(&arguments.tag, trc::location!())?,
                self.server
                    .get_property::<HashedValue<Archive>>(
                        account_id,
                        Collection::Email,
                        id,
                        &Property::Keywords,
                    )
                    .await
                    .imap_ctx(&arguments.tag, trc::location!())?,
            ) {
                (email, keywords)
            } else {
                trc::event!(
                    Store(trc::StoreEvent::NotFound),
                    AccountId = account_id,
                    DocumentId = id,
                    Collection = Collection::Email,
                    Details = "Message metadata not found.",
                    CausedBy = trc::location!(),
                );
                continue;
            };
            let email = email_
                .unarchive::<ArchivedMessageMetadata>()
                .imap_ctx(&arguments.tag, trc::location!())?;
            let keywords = keywords_
                .inner
                .unarchive::<ArchivedVec<ArchivedKeyword>>()
                .imap_ctx(&arguments.tag, trc::location!())?;

            // Fetch and parse blob
            let raw_message: Cow<[u8]> = if needs_blobs {
                // Retrieve raw message if needed
                match self
                    .server
                    .blob_store()
                    .get_blob(email.blob_hash.0.as_slice(), 0..usize::MAX)
                    .await
                    .imap_ctx(&arguments.tag, trc::location!())?
                {
                    Some(raw_message) => raw_message.into(),
                    None => {
                        trc::event!(
                            Store(trc::StoreEvent::NotFound),
                            AccountId = account_id,
                            DocumentId = id,
                            Collection = Collection::Email,
                            BlobId = email.blob_hash.0.as_slice(),
                            Details = "Blob not found.",
                            CausedBy = trc::location!(),
                        );

                        continue;
                    }
                }
            } else {
                email.raw_headers.as_slice().into()
            };
            let message = &email.contents;
            let decoded = message.decode_contents(raw_message.as_ref());

            // Build response
            let mut items = Vec::with_capacity(arguments.attributes.len());
            let set_seen_flag =
                set_seen_flags && !keywords.iter().any(|k| k == &ArchivedKeyword::Seen);
            let thread_id = if needs_thread_id || set_seen_flag {
                if let Some(thread_id) = self
                    .server
                    .get_property::<u32>(account_id, Collection::Email, id, Property::ThreadId)
                    .await
                    .imap_ctx(&arguments.tag, trc::location!())?
                {
                    thread_id
                } else {
                    continue;
                }
            } else {
                0
            };
            for attribute in &arguments.attributes {
                match attribute {
                    Attribute::Envelope => {
                        items.push(DataItem::Envelope {
                            envelope: message.envelope(),
                        });
                    }
                    Attribute::Flags => {
                        let mut flags = keywords.iter().map(Flag::from).collect::<Vec<_>>();
                        if set_seen_flag {
                            flags.push(Flag::Seen);
                        }
                        items.push(DataItem::Flags { flags });
                    }
                    Attribute::InternalDate => {
                        items.push(DataItem::InternalDate {
                            date: u64::from(email.received_at) as i64,
                        });
                    }
                    Attribute::Preview { .. } => {
                        items.push(DataItem::Preview {
                            contents: if !email.preview.is_empty() {
                                Some(email.preview.as_bytes().into())
                            } else {
                                None
                            },
                        });
                    }
                    Attribute::Rfc822Size => {
                        items.push(DataItem::Rfc822Size {
                            size: u32::from(email.size) as usize,
                        });
                    }
                    Attribute::Uid => {
                        items.push(DataItem::Uid { uid });
                    }
                    Attribute::Rfc822 => {
                        items.push(DataItem::Rfc822 {
                            contents: raw_message.as_ref().into(),
                        });
                    }
                    Attribute::Rfc822Header => {
                        let message = email.root_part();
                        if let Some(header) = raw_message.get(
                            u32::from(message.offset_header) as usize
                                ..u32::from(message.offset_body) as usize,
                        ) {
                            items.push(DataItem::Rfc822Header {
                                contents: header.into(),
                            });
                        }
                    }
                    Attribute::Rfc822Text => {
                        items.push(DataItem::Rfc822Text {
                            contents: raw_message.as_ref().into(),
                        });
                    }
                    Attribute::Body => {
                        items.push(DataItem::Body {
                            part: message.body_structure(&decoded, false),
                        });
                    }
                    Attribute::BodyStructure => {
                        items.push(DataItem::BodyStructure {
                            part: message.body_structure(&decoded, true),
                        });
                    }
                    Attribute::BodySection {
                        sections, partial, ..
                    } => {
                        if let Some(contents) = message.body_section(&decoded, sections, *partial) {
                            items.push(DataItem::BodySection {
                                sections: sections.to_vec(),
                                origin_octet: partial.map(|(start, _)| start),
                                contents,
                            });
                        }
                    }

                    Attribute::Binary {
                        sections, partial, ..
                    } => match message.binary(&decoded, sections, *partial) {
                        Ok(Some(contents)) => {
                            items.push(DataItem::Binary {
                                sections: sections.to_vec(),
                                offset: partial.map(|(start, _)| start),
                                contents,
                            });
                        }
                        Err(_) => {
                            self.write_error(
                                trc::ImapEvent::Error
                                    .into_err()
                                    .details(format!(
                                        "Failed to decode part {} of message {}.",
                                        sections
                                            .iter()
                                            .map(|s| s.to_string())
                                            .collect::<Vec<_>>()
                                            .join("."),
                                        if is_uid { uid } else { seqnum }
                                    ))
                                    .code(ResponseCode::UnknownCte),
                            )
                            .await?;
                            continue;
                        }
                        _ => (),
                    },
                    Attribute::BinarySize { sections } => {
                        if let Some(size) = message.binary_size(&decoded, sections) {
                            items.push(DataItem::BinarySize {
                                sections: sections.to_vec(),
                                size,
                            });
                        }
                    }
                    Attribute::ModSeq => {
                        if let Ok(Some(modseq)) = self
                            .server
                            .get_property::<u64>(account_id, Collection::Email, id, Property::Cid)
                            .await
                        {
                            items.push(DataItem::ModSeq { modseq: modseq + 1 });
                        }
                    }
                    Attribute::EmailId => {
                        items.push(DataItem::EmailId {
                            email_id: Id::from_parts(account_id, id).to_string(),
                        });
                    }
                    Attribute::ThreadId => {
                        items.push(DataItem::ThreadId {
                            thread_id: Id::from_parts(account_id, thread_id).to_string(),
                        });
                    }
                }
            }

            // Add flags to the response if the message was unseen
            if set_seen_flag && !arguments.attributes.contains(&Attribute::Flags) {
                let mut flags = keywords.iter().map(Flag::from).collect::<Vec<_>>();
                flags.push(Flag::Seen);
                items.push(DataItem::Flags { flags });
            }

            // Serialize fetch item
            let mut buf = Vec::with_capacity(128);
            FetchItem { id: seqnum, items }.serialize(&mut buf);
            self.write_bytes(buf).await?;

            // Add to set flags
            if set_seen_flag {
                set_seen_ids.push((
                    Id::from_parts(thread_id, id),
                    HashedValue {
                        hash: keywords_.hash,
                        inner: rkyv_deserialize::<_, Vec<Keyword>>(keywords)
                            .imap_ctx(&arguments.tag, trc::location!())?,
                    },
                ));
            }
        }

        // Set Seen ids
        if !set_seen_ids.is_empty() {
            let mut changelog = self
                .server
                .begin_changes(account_id)
                .imap_ctx(&arguments.tag, trc::location!())?;
            for (id, mut keywords) in set_seen_ids {
                keywords.inner.push(Keyword::Seen);
                let mut batch = BatchBuilder::new();
                batch
                    .with_account_id(account_id)
                    .with_collection(Collection::Email)
                    .update_document(id.document_id())
                    .assert_value(Property::Keywords, &keywords)
                    .set(
                        Property::Keywords,
                        Archiver::new(keywords.inner)
                            .serialize()
                            .imap_ctx(&arguments.tag, trc::location!())?,
                    )
                    .tag(Property::Keywords, Keyword::Seen)
                    .set(Property::Cid, changelog.change_id.serialize());
                match self
                    .server
                    .store()
                    .write(batch)
                    .await
                    .imap_ctx(&arguments.tag, trc::location!())
                {
                    Ok(_) => {
                        changelog.log_update(Collection::Email, id);
                    }
                    Err(err) => {
                        if !err.is_assertion_failure() {
                            return Err(err.id(arguments.tag));
                        }
                    }
                }
            }
            if !changelog.is_empty() {
                // Write changes
                let change_id = self
                    .server
                    .commit_changes(account_id, changelog)
                    .await
                    .imap_ctx(&arguments.tag, trc::location!())?;
                modseq = change_id.into();
                self.server
                    .broadcast_state_change(
                        StateChange::new(account_id).with_change(DataType::Email, change_id),
                    )
                    .await;
            }
        }

        trc::event!(
            Imap(trc::ImapEvent::Fetch),
            SpanId = self.session_id,
            AccountId = account_id,
            MailboxId = mailbox.id.mailbox_id,
            DocumentId = fetched_ids,
            Details = arguments
                .attributes
                .iter()
                .map(|c| trc::Value::from(format!("{c:?}")))
                .collect::<Vec<_>>(),
            Elapsed = op_start.elapsed()
        );

        // Condstore was enabled with this command
        if enabled_condstore {
            self.write_bytes(
                StatusResponse::ok("Highest Modseq")
                    .with_code(ResponseCode::highest_modseq(modseq))
                    .into_bytes(),
            )
            .await?;
        }

        Ok(StatusResponse::completed(Command::Fetch(is_uid)).with_tag(arguments.tag))
    }
}

#[allow(clippy::result_unit_err)]
pub trait AsImapDataItem {
    fn body_structure(&self, decoded: &DecodedParts<'_>, is_extended: bool) -> BodyPart;
    fn body_section<'x>(
        &self,
        decoded: &'x DecodedParts<'x>,
        sections: &[Section],
        partial: Option<(u32, u32)>,
    ) -> Option<Cow<'x, [u8]>>;
    fn binary<'x>(
        &self,
        decoded: &'x DecodedParts<'x>,
        sections: &[u32],
        partial: Option<(u32, u32)>,
    ) -> Result<Option<BodyContents<'x>>, ()>;
    fn binary_size(&self, decoded: &DecodedParts<'_>, sections: &[u32]) -> Option<usize>;
    fn as_body_part(
        &self,
        decoded: &DecodedParts<'_>,
        message_id: usize,
        part_id: usize,
        is_extended: bool,
    ) -> BodyPart;
    fn envelope(&self) -> Envelope;
}

impl AsImapDataItem for ArchivedMessageMetadataContents {
    fn body_structure(&self, decoded: &DecodedParts<'_>, is_extended: bool) -> BodyPart {
        let mut stack = Vec::new();
        let base_part = [u16_le::from_native(0)];
        let mut parts = base_part.as_slice().iter();
        let mut message = self;
        let mut root_part = None;
        let mut message_id = 0;

        loop {
            while let Some(part_id) = parts.next() {
                let part_id = u16::from(part_id) as usize;
                let mut part = message.as_body_part(decoded, message_id, part_id, is_extended);

                match &message.parts[part_id].body {
                    ArchivedMetadataPartType::Message(nested_message) => {
                        part.set_envelope(nested_message.envelope());
                        if let Some(root_part) = root_part {
                            stack.push((root_part, parts, (message, message_id).into()));
                            message_id += 1;
                        }
                        root_part = part.into();
                        parts = base_part.as_slice().iter();
                        message = nested_message;
                        continue;
                    }
                    ArchivedMetadataPartType::Multipart(subparts) => {
                        if let Some(root_part) = root_part {
                            stack.push((root_part, parts, None));
                        }
                        root_part = part.into();
                        parts = subparts.iter();
                        continue;
                    }
                    _ => (),
                }
                if let Some(root_part) = &mut root_part {
                    root_part.add_part(part);
                } else {
                    return part;
                }
            }
            if let Some((mut prev_root_part, prev_parts, prev_message)) = stack.pop() {
                if let Some((prev_message, prev_message_id)) = prev_message {
                    message = prev_message;
                    message_id = prev_message_id;
                }

                prev_root_part.add_part(root_part.unwrap());
                parts = prev_parts;
                root_part = prev_root_part.into();
            } else {
                break;
            }
        }

        root_part.unwrap()
    }

    fn as_body_part(
        &self,
        decoded: &DecodedParts<'_>,
        message_id: usize,
        part_id: usize,
        is_extended: bool,
    ) -> BodyPart {
        let part = &self.parts[part_id];
        let body = decoded.raw_message_section_arch(message_id, part.offset_body, part.offset_end);
        let (is_multipart, is_text) = match &part.body {
            ArchivedMetadataPartType::Text | ArchivedMetadataPartType::Html => (false, true),
            ArchivedMetadataPartType::Multipart(_) => (true, false),
            _ => (false, false),
        };
        let content_type = part
            .headers
            .header_value(&ArchivedHeaderName::ContentType)
            .and_then(|ct| ct.as_content_type());

        let mut body_md5 = None;
        let mut extension = BodyPartExtension::default();
        let mut fields = BodyPartFields::default();

        if !is_multipart || is_extended {
            fields.body_parameters = content_type.as_ref().and_then(|ct| {
                ct.attributes.as_ref().map(|at| {
                    at.iter()
                        .map(|k| (k.0.as_ref().into(), k.1.as_ref().into()))
                        .collect::<Vec<_>>()
                })
            })
        }

        if !is_multipart {
            fields.body_subtype = content_type
                .as_ref()
                .and_then(|ct| ct.c_subtype.as_ref().map(|cs| cs.as_ref().into()));

            fields.body_id = part
                .headers
                .header_value(&ArchivedHeaderName::ContentId)
                .and_then(|id| id.as_text().map(|id| format!("<{}>", id).into()));

            fields.body_description = part
                .headers
                .header_value(&ArchivedHeaderName::ContentDescription)
                .and_then(|ct| ct.as_text().map(|ct| ct.into()));

            fields.body_encoding = part
                .headers
                .header_value(&ArchivedHeaderName::ContentTransferEncoding)
                .and_then(|ct| ct.as_text().map(|ct| ct.into()));

            fields.body_size_octets = body.as_ref().map(|b| b.len()).unwrap_or(0);

            if is_text {
                if fields.body_subtype.is_none() {
                    fields.body_subtype = Some("plain".into());
                }
                if fields.body_encoding.is_none() {
                    fields.body_encoding = Some("7bit".into());
                }
                if fields.body_parameters.is_none() {
                    fields.body_parameters = Some(vec![("charset".into(), "us-ascii".into())]);
                }
            }
        }

        if is_extended {
            if !is_multipart {
                body_md5 = body
                    .as_ref()
                    .map(|b| format!("{:x}", md5::compute(b)).into());
            }

            extension.body_disposition = part
                .headers
                .header_value(&ArchivedHeaderName::ContentDisposition)
                .and_then(|cd| cd.as_content_type())
                .map(|cd| {
                    (
                        cd.c_type.as_ref().into(),
                        cd.attributes
                            .as_ref()
                            .map(|at| {
                                at.iter()
                                    .map(|k| (k.0.as_ref().into(), k.1.as_ref().into()))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default(),
                    )
                });

            extension.body_language = part
                .headers
                .header_value(&ArchivedHeaderName::ContentLanguage)
                .and_then(|hv| {
                    hv.as_text_list()
                        .map(|list| list.iter().map(|item| item.as_ref().into()).collect())
                });

            extension.body_location = part
                .headers
                .header_value(&ArchivedHeaderName::ContentLocation)
                .and_then(|ct| ct.as_text().map(|ct| ct.into()));
        }

        match &part.body {
            ArchivedMetadataPartType::Multipart(parts) => BodyPart::Multipart {
                body_parts: Vec::with_capacity(parts.len()),
                body_subtype: content_type
                    .as_ref()
                    .and_then(|ct| ct.c_subtype.as_ref().map(|cs| cs.as_ref().into()))
                    .unwrap_or_else(|| "".into()),
                body_parameters: fields.body_parameters,
                extension,
            },
            ArchivedMetadataPartType::Message(_) => BodyPart::Message {
                fields,
                envelope: None,
                body: None,
                body_size_lines: 0,
                body_md5,
                extension,
            },
            _ => {
                if is_text {
                    BodyPart::Text {
                        fields,
                        body_size_lines: body
                            .as_ref()
                            .map(|b| b.iter().filter(|&&ch| ch == b'\n').count())
                            .unwrap_or(0),
                        body_md5,
                        extension,
                    }
                } else {
                    BodyPart::Basic {
                        body_type: content_type
                            .as_ref()
                            .map(|ct| Cow::from(ct.c_type.as_ref())),
                        fields,
                        body_md5,
                        extension,
                    }
                }
            }
        }
    }

    fn body_section<'x>(
        &self,
        decoded: &'x DecodedParts<'x>,
        sections: &[Section],
        partial: Option<(u32, u32)>,
    ) -> Option<Cow<'x, [u8]>> {
        let mut part = self.root_part();
        if sections.is_empty() {
            return Some(
                get_partial_bytes(
                    decoded.raw_message_section_arch(0, part.offset_header, part.offset_end)?,
                    partial,
                )
                .into(),
            );
        }

        let mut message = self;
        let mut message_id = 0;
        let mut sections_iter = sections.iter().enumerate().peekable();

        while let Some((section_num, section)) = sections_iter.next() {
            match section {
                Section::Part { num } => {
                    part = if let Some(sub_part_ids) = part.sub_parts() {
                        sub_part_ids
                            .get((*num).saturating_sub(1) as usize)
                            .and_then(|pos| message.parts.get(u16::from(*pos) as usize))
                    } else if *num == 1 && (section_num == sections.len() - 1 || part.is_message())
                    {
                        Some(part)
                    } else {
                        None
                    }?;

                    if let (
                        ArchivedMetadataPartType::Message(nested_message),
                        Some((
                            _,
                            Section::Part { .. }
                            | Section::Header
                            | Section::HeaderFields { .. }
                            | Section::Text,
                        )),
                    ) = (&part.body, sections_iter.peek())
                    {
                        message = nested_message;
                        part = message.root_part();
                        message_id += 1;
                    }
                }
                Section::Header => {
                    return Some(
                        get_partial_bytes(
                            decoded.raw_message_section_arch(
                                message_id,
                                part.offset_header,
                                part.offset_body,
                            )?,
                            partial,
                        )
                        .into(),
                    );
                }
                Section::HeaderFields { not, fields } => {
                    let mut headers = Vec::with_capacity(
                        u32::from(part.offset_body).saturating_sub(u32::from(part.offset_header))
                            as usize,
                    );
                    for header in part.headers.iter() {
                        let header_name = header.name.as_str();
                        if fields.iter().any(|f| header_name.eq_ignore_ascii_case(f)) != *not {
                            headers.extend_from_slice(header_name.as_bytes());
                            headers.push(b':');
                            headers.extend_from_slice(
                                decoded
                                    .raw_message_section_arch(
                                        message_id,
                                        header.offset_start,
                                        header.offset_end,
                                    )
                                    .unwrap_or_default(),
                            );
                        }
                    }

                    headers.extend_from_slice(b"\r\n");

                    return Some(if partial.is_none() {
                        headers.into()
                    } else {
                        get_partial_bytes(&headers, partial).to_vec().into()
                    });
                }
                Section::Text => {
                    return Some(
                        get_partial_bytes(
                            decoded.raw_message_section_arch(
                                message_id,
                                part.offset_body,
                                part.offset_end,
                            )?,
                            partial,
                        )
                        .into(),
                    );
                }
                Section::Mime => {
                    let mut headers = Vec::with_capacity(
                        u32::from(part.offset_body).saturating_sub(u32::from(part.offset_header))
                            as usize,
                    );
                    for header in part.headers.iter() {
                        if header.name.is_mime_header()
                            || header.name.as_str().starts_with("Content-")
                        {
                            headers.extend_from_slice(header.name.as_str().as_bytes());
                            headers.extend_from_slice(b":");
                            headers.extend_from_slice(
                                decoded
                                    .raw_message_section_arch(
                                        message_id,
                                        header.offset_start,
                                        header.offset_end,
                                    )
                                    .unwrap_or_default(),
                            );
                        }
                    }
                    headers.extend_from_slice(b"\r\n");
                    return Some(if partial.is_none() {
                        headers.into()
                    } else {
                        get_partial_bytes(&headers, partial).to_vec().into()
                    });
                }
            }
        }

        // BODY[x] should return both headers and body, but most clients
        // expect BODY[x] to return only the body, just like BOXY[x.TEXT] does.

        Some(
            get_partial_bytes(
                decoded.raw_message_section_arch(message_id, part.offset_body, part.offset_end)?,
                partial,
            )
            .into(),
        )
    }

    fn binary<'x>(
        &self,
        decoded: &'x DecodedParts<'x>,
        sections: &[u32],
        partial: Option<(u32, u32)>,
    ) -> Result<Option<BodyContents<'x>>, ()> {
        let mut message = self;
        let mut message_id = 0;
        let mut part = self.root_part();
        let mut part_id = 0;
        let mut sections_iter = sections.iter().enumerate().peekable();

        while let Some((section_num, num)) = sections_iter.next() {
            part = if let Some(sub_part_ids) = part.sub_parts() {
                part_id = (*num).saturating_sub(1) as usize;
                if let Some(part) = sub_part_ids
                    .get(part_id)
                    .and_then(|pos| message.parts.get(u16::from(*pos) as usize))
                {
                    part
                } else {
                    return Ok(None);
                }
            } else if *num == 1 && (section_num == sections.len() - 1 || part.is_message()) {
                part_id = 0;
                part
            } else {
                return Ok(None);
            };

            if let (ArchivedMetadataPartType::Message(nested_message), Some(_)) =
                (&part.body, sections_iter.peek())
            {
                message = nested_message;
                part = message.root_part();
                message_id += 1;
            }
        }

        if !part.is_encoding_problem {
            Ok(match &part.body {
                ArchivedMetadataPartType::Text | ArchivedMetadataPartType::Html => {
                    BodyContents::Text(String::from_utf8_lossy(get_partial_bytes(
                        decoded.binary_part(message_id, part_id).unwrap_or_default(),
                        partial,
                    )))
                    .into()
                }
                ArchivedMetadataPartType::Binary | ArchivedMetadataPartType::InlineBinary => {
                    BodyContents::Bytes(
                        get_partial_bytes(
                            decoded.binary_part(message_id, part_id).unwrap_or_default(),
                            partial,
                        )
                        .into(),
                    )
                    .into()
                }
                ArchivedMetadataPartType::Message(message) => BodyContents::Bytes({
                    {
                        let part = message.root_part();
                        get_partial_bytes(
                            decoded
                                .raw_message_section_arch(
                                    message_id,
                                    part.offset_header,
                                    part.offset_end,
                                )
                                .unwrap_or_default(),
                            partial,
                        )
                        .into()
                    }
                })
                .into(),
                ArchivedMetadataPartType::Multipart(_) => BodyContents::Bytes(
                    get_partial_bytes(
                        decoded
                            .raw_message_section_arch(
                                message_id,
                                part.offset_header,
                                part.offset_end,
                            )
                            .unwrap_or_default(),
                        partial,
                    )
                    .into(),
                )
                .into(),
            })
        } else {
            Err(())
        }
    }

    fn binary_size(&self, decoded: &DecodedParts<'_>, sections: &[u32]) -> Option<usize> {
        let mut message = self;
        let mut message_id = 0;
        let mut part = self.root_part();
        let mut part_id = 0;
        let mut sections_iter = sections.iter().enumerate().peekable();

        while let Some((section_num, num)) = sections_iter.next() {
            part_id = (*num).saturating_sub(1) as usize;
            part = if let Some(sub_part_ids) = part.sub_parts() {
                sub_part_ids
                    .get(part_id)
                    .and_then(|pos| message.parts.get(u16::from(pos) as usize))
            } else if *num == 1 && (section_num == sections.len() - 1 || part.is_message()) {
                Some(part)
            } else {
                None
            }?;

            if let (ArchivedMetadataPartType::Message(nested_message), Some(_)) =
                (&part.body, sections_iter.peek())
            {
                message = nested_message;
                message_id += 1;
                part = message.root_part();
                part_id = 0;
            }
        }

        match &part.body {
            ArchivedMetadataPartType::Text
            | ArchivedMetadataPartType::Html
            | ArchivedMetadataPartType::Binary
            | ArchivedMetadataPartType::InlineBinary => decoded
                .part(message_id, part_id)
                .map(|p| p.len())
                .unwrap_or_default(),
            ArchivedMetadataPartType::Message(message) => message.root_part().raw_len(),
            ArchivedMetadataPartType::Multipart(_) => part.raw_len(),
        }
        .into()
    }

    fn envelope(&self) -> Envelope {
        let headers = self.root_part();
        Envelope {
            date: headers.date().map(DateTime::from_timestamp),
            subject: headers.subject().map(|s| s.into()),
            from: headers
                .header_values(ArchivedHeaderName::From)
                .flat_map(|a| a.as_imap_address())
                .collect(),
            sender: headers
                .header_values(ArchivedHeaderName::Sender)
                .flat_map(|a| a.as_imap_address())
                .collect(),
            reply_to: headers
                .header_values(ArchivedHeaderName::ReplyTo)
                .flat_map(|a| a.as_imap_address())
                .collect(),
            to: headers
                .header_values(ArchivedHeaderName::To)
                .flat_map(|a| a.as_imap_address())
                .collect(),
            cc: headers
                .header_values(ArchivedHeaderName::Cc)
                .flat_map(|a| a.as_imap_address())
                .collect(),
            bcc: headers
                .header_values(ArchivedHeaderName::Bcc)
                .flat_map(|a| a.as_imap_address())
                .collect(),
            in_reply_to: headers.in_reply_to().as_text_list().map(|list| {
                let mut irt = String::with_capacity(list.len() * 10);
                for (pos, l) in list.iter().enumerate() {
                    if pos > 0 {
                        irt.push(' ');
                    }
                    irt.push('<');
                    irt.push_str(l.as_ref());
                    irt.push('>');
                }
                irt.into()
            }),
            message_id: headers.message_id().map(|id| format!("<{}>", id).into()),
        }
    }
}

#[inline(always)]
fn get_partial_bytes(bytes: &[u8], partial: Option<(u32, u32)>) -> &[u8] {
    if let Some((start, end)) = partial {
        bytes
            .get(start as usize..std::cmp::min((start + end) as usize, bytes.len()))
            .unwrap_or_default()
    } else {
        bytes
    }
}

trait AsImapAddress {
    fn as_imap_address(&self) -> Vec<fetch::Address>;
}

impl AsImapAddress for ArchivedHeaderValue {
    fn as_imap_address(&self) -> Vec<fetch::Address> {
        let mut addresses = Vec::new();

        match self {
            ArchivedHeaderValue::Address(ArchivedAddress::List(list)) => {
                for addr in list.iter() {
                    if let Some(email) = addr.address.as_ref() {
                        addresses.push(fetch::Address::Single(fetch::EmailAddress {
                            name: addr.name.as_ref().map(|n| n.as_ref().into()),
                            address: email.as_ref().into(),
                        }));
                    }
                }
            }
            ArchivedHeaderValue::Address(ArchivedAddress::Group(list)) => {
                for group in list.iter() {
                    addresses.push(fetch::Address::Group(fetch::AddressGroup {
                        name: group.name.as_ref().map(|n| n.as_ref().into()),
                        addresses: group
                            .addresses
                            .iter()
                            .filter_map(|addr| {
                                fetch::EmailAddress {
                                    name: addr.name.as_ref().map(|n| n.as_ref().into()),
                                    address: addr.address.as_ref()?.as_ref().into(),
                                }
                                .into()
                            })
                            .collect(),
                    }));
                }
            }
            _ => (),
        }

        addresses
    }
}
