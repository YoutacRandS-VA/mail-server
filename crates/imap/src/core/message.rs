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

use std::{collections::BTreeMap, sync::Arc};

use ahash::AHashMap;
use imap_proto::{
    protocol::{expunge, select::Exists, Sequence},
    StatusResponse,
};
use jmap::mailbox::UidMailbox;
use jmap_proto::{
    object::Object,
    types::{collection::Collection, property::Property, value::Value},
};
use store::write::assert::HashedValue;
use utils::{listener::SessionStream, lru_cache::LruCached};

use crate::core::ImapId;

use super::{ImapUidToId, MailboxId, MailboxState, NextMailboxState, SelectedMailbox, SessionData};

pub(crate) const MAX_RETRIES: usize = 10;

impl<T: SessionStream> SessionData<T> {
    pub async fn fetch_messages(&self, mailbox: &MailboxId) -> crate::op::Result<MailboxState> {
        // Obtain message ids
        let message_ids = self
            .jmap
            .get_tag(
                mailbox.account_id,
                Collection::Email,
                Property::MailboxIds,
                mailbox.mailbox_id,
            )
            .await?
            .unwrap_or_default();

        // Obtain UID validity
        let uid_validity = self.get_uid_validity(mailbox).await?;

        // Obtain current state
        let modseq = self
            .jmap
            .store
            .get_last_change_id(mailbox.account_id, Collection::Email)
            .await
            .map_err(|err| {
                tracing::error!(event = "error",
                    context = "store",
                    account_id = mailbox.account_id,
                    collection = ?Collection::Email,
                    error = ?err,
                    "Failed to obtain state");
                StatusResponse::database_failure()
            })?;

        // Obtain all message ids
        let mut uid_map = BTreeMap::new();
        for (message_id, uid_mailbox) in self
            .jmap
            .get_properties::<HashedValue<Vec<UidMailbox>>, _, _>(
                mailbox.account_id,
                Collection::Email,
                &message_ids,
                Property::MailboxIds,
            )
            .await?
            .into_iter()
        {
            // Make sure the message is still in this mailbox
            if let Some(item) = uid_mailbox
                .inner
                .iter()
                .find(|item| item.mailbox_id == mailbox.mailbox_id)
            {
                debug_assert!(item.uid != 0, "UID is zero for message {item:?}");
                if uid_map.insert(item.uid, message_id).is_some() {
                    tracing::warn!(event = "error",
                            context = "store",
                            account_id = mailbox.account_id,
                            collection = ?Collection::Mailbox,
                            mailbox_id = mailbox.mailbox_id,
                            message_id = message_id,
                            "Duplicate UID");
                }
            }
        }

        // Obtain UID next and assign UIDs
        let mut uid_max = 0;
        let mut id_to_imap = AHashMap::with_capacity(uid_map.len());
        let mut uid_to_id = AHashMap::with_capacity(uid_map.len());

        for (seqnum, (uid, message_id)) in uid_map.into_iter().enumerate() {
            if uid > uid_max {
                uid_max = uid;
            }
            id_to_imap.insert(
                message_id,
                ImapId {
                    uid,
                    seqnum: seqnum as u32 + 1,
                },
            );
            uid_to_id.insert(uid, message_id);
        }

        Ok(MailboxState {
            uid_next: uid_max + 1,
            uid_validity,
            total_messages: id_to_imap.len(),
            id_to_imap,
            uid_to_id,
            uid_max,
            modseq,
            next_state: None,
        })
    }

    pub async fn synchronize_messages(
        &self,
        mailbox: &SelectedMailbox,
    ) -> crate::op::Result<Option<u64>> {
        // Obtain current modseq
        let modseq = self.get_modseq(mailbox.id.account_id).await?;
        if mailbox.state.lock().modseq != modseq {
            // Synchronize messages
            let new_state = self.fetch_messages(&mailbox.id).await?;
            let mut current_state = mailbox.state.lock();

            // Add missing uids
            let mut deletions = current_state
                .next_state
                .take()
                .map(|state| state.deletions)
                .unwrap_or_default();
            let mut id_to_imap = AHashMap::with_capacity(current_state.id_to_imap.len());
            for (id, imap_id) in std::mem::take(&mut current_state.id_to_imap) {
                if !new_state.uid_to_id.contains_key(&imap_id.uid) {
                    // Add to deletions
                    deletions.push(imap_id);

                    // Invalidate entries
                    current_state.uid_to_id.remove(&imap_id.uid);
                } else {
                    id_to_imap.insert(id, imap_id);
                }
            }
            current_state.id_to_imap = id_to_imap;

            // Update cache
            self.imap
                .cache_mailbox
                .insert(mailbox.id, Arc::new(new_state.clone()));

            // Update state
            current_state.modseq = new_state.modseq;
            current_state.next_state = Some(Box::new(NextMailboxState {
                next_state: new_state,
                deletions,
            }));
        }

        Ok(modseq)
    }

    pub async fn write_mailbox_changes(
        &self,
        mailbox: &SelectedMailbox,
        is_qresync: bool,
    ) -> crate::op::Result<Option<u64>> {
        // Resync mailbox
        let modseq = self.synchronize_messages(mailbox).await?;
        let mut buf = Vec::new();
        {
            let mut current_state = mailbox.state.lock();
            if let Some(next_state) = current_state.next_state.take() {
                if !next_state.deletions.is_empty() {
                    let mut ids = next_state
                        .deletions
                        .into_iter()
                        .map(|id| if is_qresync { id.uid } else { id.seqnum })
                        .collect::<Vec<u32>>();
                    ids.sort_unstable();
                    expunge::Response { is_qresync, ids }.serialize_to(&mut buf);
                }
                if !buf.is_empty()
                    || next_state
                        .next_state
                        .uid_max
                        .saturating_sub(current_state.uid_max)
                        > 0
                {
                    Exists {
                        total_messages: next_state.next_state.total_messages,
                    }
                    .serialize(&mut buf);
                }
                *current_state = next_state.next_state;
            }
        }
        if !buf.is_empty() {
            self.write_bytes(buf).await;
        }

        Ok(modseq)
    }

    pub async fn get_modseq(&self, account_id: u32) -> crate::op::Result<Option<u64>> {
        // Obtain current modseq
        if let Ok(modseq) = self
            .jmap
            .store
            .get_last_change_id(account_id, Collection::Email)
            .await
        {
            Ok(modseq)
        } else {
            tracing::error!(parent: &self.span,
                event = "error",
                context = "store",
                account_id = account_id,
                collection = ?Collection::Email,
                "Failed to obtain modseq");
            Err(StatusResponse::database_failure())
        }
    }

    pub async fn get_uid_validity(&self, mailbox: &MailboxId) -> crate::op::Result<u32> {
        self.jmap
            .get_property::<Object<Value>>(
                mailbox.account_id,
                Collection::Mailbox,
                mailbox.mailbox_id,
                &Property::Value,
            )
            .await?
            .and_then(|obj| obj.get(&Property::Cid).as_uint())
            .ok_or_else(|| {
                tracing::debug!(event = "error",
                context = "store",
                account_id = mailbox.account_id,
                collection = ?Collection::Mailbox,
                mailbox_id = mailbox.mailbox_id,
                "Failed to obtain uid validity");
                StatusResponse::no("Mailbox unavailable.")
            })
            .map(|v| v as u32)
    }
}

impl SelectedMailbox {
    pub async fn sequence_to_ids(
        &self,
        sequence: &Sequence,
        is_uid: bool,
    ) -> crate::op::Result<AHashMap<u32, ImapId>> {
        if !sequence.is_saved_search() {
            let mut ids = AHashMap::new();
            let state = self.state.lock();
            if state.id_to_imap.is_empty() {
                return Ok(ids);
            }

            if is_uid {
                for (id, imap_id) in &state.id_to_imap {
                    if sequence.contains(imap_id.uid, state.uid_max) {
                        ids.insert(*id, *imap_id);
                    }
                }
            } else {
                for (id, imap_id) in &state.id_to_imap {
                    if sequence.contains(imap_id.seqnum, state.total_messages as u32) {
                        ids.insert(*id, *imap_id);
                    }
                }
            }

            Ok(ids)
        } else {
            let saved_ids = self
                .get_saved_search()
                .await
                .ok_or_else(|| StatusResponse::no("No saved search found."))?;
            let mut ids = AHashMap::with_capacity(saved_ids.len());
            let state = self.state.lock();

            for imap_id in saved_ids.iter() {
                if let Some(id) = state.uid_to_id.get(&imap_id.uid) {
                    ids.insert(*id, *imap_id);
                }
            }

            Ok(ids)
        }
    }

    pub async fn sequence_expand_missing(&self, sequence: &Sequence, is_uid: bool) -> Vec<u32> {
        let mut deleted_ids = Vec::new();
        if !sequence.is_saved_search() {
            let state = self.state.lock();
            if is_uid {
                for uid in sequence.expand(state.uid_max) {
                    if !state.uid_to_id.contains_key(&uid) {
                        deleted_ids.push(uid);
                    }
                }
            } else {
                for seqnum in sequence.expand(state.total_messages as u32) {
                    if seqnum > state.total_messages as u32 {
                        deleted_ids.push(seqnum);
                    }
                }
            }
        } else if let Some(saved_ids) = self.get_saved_search().await {
            let state = self.state.lock();
            for id in saved_ids.iter() {
                if !state.uid_to_id.contains_key(&id.uid) {
                    deleted_ids.push(if is_uid { id.uid } else { id.seqnum });
                }
            }
        }
        deleted_ids.sort_unstable();
        deleted_ids
    }

    pub fn append_messages(&self, ids: Vec<ImapUidToId>, modseq: Option<u64>) -> u32 {
        let mut mailbox = self.state.lock();
        if modseq.unwrap_or(0) > mailbox.modseq.unwrap_or(0) {
            let mut uid_max = 0;
            for id in ids {
                mailbox.total_messages += 1;
                let seqnum = mailbox.total_messages as u32;
                mailbox.uid_to_id.insert(id.uid, id.uid);
                mailbox.id_to_imap.insert(
                    id.id,
                    ImapId {
                        uid: id.uid,
                        seqnum,
                    },
                );
                uid_max = id.uid;
            }
            mailbox.uid_max = uid_max;
            mailbox.uid_next = uid_max + 1;
        }
        mailbox.uid_validity
    }
}
