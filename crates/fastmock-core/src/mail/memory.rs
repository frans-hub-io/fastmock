use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::model::{MailEntry, MailMessage};
use super::query::{MailCursor, MailFilter, MailPage, MailQuery};
use super::store::*;
use crate::model::SpaceFilter;
use crate::storage::StoreResult;

type Area = Option<String>;
type Key = (DateTime<Utc>, Uuid);

struct Slot {
    message_id: Uuid,
    flags: BTreeSet<String>,
}

struct Folder {
    uid_validity: u32,
    uid_next: u32,
    entries: BTreeMap<u32, Slot>,
}

#[derive(Default)]
struct Mailbox {
    folders: BTreeMap<String, Folder>,
    password: Option<String>,
}

#[derive(Default)]
struct State {
    areas: HashMap<Area, BTreeMap<Key, MailMessage>>,
    index: HashMap<Uuid, (Area, DateTime<Utc>)>,
    mailboxes: BTreeMap<(Area, String), Mailbox>,
    last_validity: u32,
}

impl State {
    fn new_folder(&mut self) -> Folder {
        let now = u32::try_from(Utc::now().timestamp()).unwrap_or(u32::MAX - 1_000_000);
        self.last_validity = now.max(self.last_validity.saturating_add(1));
        Folder {
            uid_validity: self.last_validity,
            uid_next: 1,
            entries: BTreeMap::new(),
        }
    }

    fn folder(&mut self, area: &Area, address: &str, folder: &str) -> &mut Folder {
        let key = (area.clone(), address.to_string());
        if !self
            .mailboxes
            .get(&key)
            .is_some_and(|m| m.folders.contains_key(folder))
        {
            let f = self.new_folder();
            self.mailboxes
                .entry(key.clone())
                .or_default()
                .folders
                .insert(folder.to_string(), f);
        }
        self.mailboxes
            .get_mut(&key)
            .and_then(|m| m.folders.get_mut(folder))
            .expect("folder just created")
    }

    fn existing(&mut self, key: &MailboxKey) -> Option<&mut Folder> {
        self.mailboxes
            .get_mut(&(key.space.clone(), key.address.clone()))
            .and_then(|m| m.folders.get_mut(&folder_name(&key.folder)))
    }

    fn message(&self, id: Uuid) -> Option<&MailMessage> {
        let (area, at) = self.index.get(&id)?;
        self.areas.get(area)?.get(&(*at, id))
    }

    fn remove(&mut self, id: Uuid) -> Option<MailRef> {
        let (area, at) = self.index.remove(&id)?;
        if let Some(map) = self.areas.get_mut(&area) {
            map.remove(&(at, id));
        }
        for ((a, _), mb) in self.mailboxes.iter_mut() {
            if *a == area {
                for f in mb.folders.values_mut() {
                    f.entries.retain(|_, s| s.message_id != id);
                }
            }
        }
        Some(MailRef { space: area, id })
    }

    fn entry(&self, uid: u32, slot: &Slot) -> Option<MailboxEntry> {
        let m = self.message(slot.message_id)?;
        Some(MailboxEntry {
            uid,
            message_id: slot.message_id,
            flags: slot.flags.iter().cloned().collect(),
            received_at: m.received_at,
            size: m.raw.len() as u64,
        })
    }

    fn info(name: &str, f: &Folder) -> MailboxInfo {
        MailboxInfo {
            folder: name.to_string(),
            uid_validity: f.uid_validity,
            uid_next: f.uid_next,
            exists: f.entries.len() as u32,
            unseen: f
                .entries
                .values()
                .filter(|s| !s.flags.contains("\\Seen"))
                .count() as u32,
        }
    }
}

/// Mail kept in process memory; every area keeps at most `max_per_area` messages, dropping the oldest
/// when a new one arrives.
pub struct MemoryMailStore {
    state: Mutex<State>,
    max_per_area: usize,
}

impl MemoryMailStore {
    pub fn new(max_per_area: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            max_per_area: max_per_area.max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for MemoryMailStore {
    fn default() -> Self {
        Self::new(1000)
    }
}

fn flag_set(flags: &[String]) -> BTreeSet<String> {
    flags.iter().map(|f| flag_name(f)).collect()
}

fn page(query: &MailQuery, st: &State) -> MailPage {
    let limit = query.page_size();
    let mut out: Vec<&MailMessage> = Vec::new();
    for (area, map) in &st.areas {
        if !query.spaces.matches(area.as_deref()) {
            continue;
        }
        let range = match query.cursor {
            None => map.range::<Key, _>(..),
            Some(c) if query.oldest_first => {
                map.range::<Key, _>((Bound::Excluded((c.received_at, c.id)), Bound::Unbounded))
            }
            Some(c) => map.range::<Key, _>(..(c.received_at, c.id)),
        };
        let accept = |m: &&MailMessage| query.filter.accepts(&m.entry(), &m.search_text);
        if query.oldest_first {
            out.extend(range.map(|(_, m)| m).filter(accept).take(limit + 1));
        } else {
            out.extend(range.rev().map(|(_, m)| m).filter(accept).take(limit + 1));
        }
    }
    out.sort_by_key(|m| (m.received_at, m.id));
    if !query.oldest_first {
        out.reverse();
    }
    let more = out.len() > limit;
    out.truncate(limit);
    let items: Vec<MailEntry> = out.iter().map(|m| m.entry()).collect();
    MailPage {
        next: if more {
            items.last().map(MailCursor::of)
        } else {
            None
        },
        items,
    }
}

#[async_trait]
impl MailStore for MemoryMailStore {
    async fn insert(&self, msg: &MailMessage, deliveries: &[MailboxDelivery]) -> StoreResult<()> {
        let mut st = self.lock();
        if st.index.contains_key(&msg.id) {
            anyhow::bail!("mail message {} already exists", msg.id);
        }
        let area = msg.space.clone();
        st.index.insert(msg.id, (area.clone(), msg.received_at));
        st.areas
            .entry(area.clone())
            .or_default()
            .insert((msg.received_at, msg.id), msg.clone());
        for d in deliveries {
            let f = st.folder(&area, &d.address, &folder_name(&d.folder));
            let uid = f.uid_next;
            f.uid_next += 1;
            f.entries.insert(
                uid,
                Slot {
                    message_id: msg.id,
                    flags: flag_set(&d.flags),
                },
            );
        }
        loop {
            let oldest = match st.areas.get(&area) {
                Some(map) if map.len() > self.max_per_area => map.keys().next().copied(),
                _ => None,
            };
            match oldest {
                Some((_, id)) => {
                    st.remove(id);
                }
                None => break,
            }
        }
        Ok(())
    }

    async fn get(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<MailMessage>> {
        let st = self.lock();
        Ok(st
            .message(id)
            .filter(|m| spaces.matches(m.space.as_deref()))
            .cloned())
    }

    async fn list(&self, query: &MailQuery) -> StoreResult<MailPage> {
        Ok(page(query, &self.lock()))
    }

    async fn delete(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<MailRef>> {
        let mut st = self.lock();
        if !st
            .message(id)
            .is_some_and(|m| spaces.matches(m.space.as_deref()))
        {
            return Ok(None);
        }
        Ok(st.remove(id))
    }

    async fn delete_matching(
        &self,
        spaces: &SpaceFilter,
        filter: &MailFilter,
    ) -> StoreResult<Vec<MailRef>> {
        let mut st = self.lock();
        let ids: Vec<Uuid> = st
            .areas
            .iter()
            .filter(|(a, _)| spaces.matches(a.as_deref()))
            .flat_map(|(_, map)| map.values())
            .filter(|m| filter.accepts(&m.entry(), &m.search_text))
            .map(|m| m.id)
            .collect();
        Ok(ids.into_iter().filter_map(|id| st.remove(id)).collect())
    }

    async fn set_labels(
        &self,
        id: Uuid,
        spaces: &SpaceFilter,
        labels: &[String],
    ) -> StoreResult<Option<MailEntry>> {
        let mut st = self.lock();
        let Some((area, at)) = st.index.get(&id).cloned() else {
            return Ok(None);
        };
        if !spaces.matches(area.as_deref()) {
            return Ok(None);
        }
        let m = st
            .areas
            .get_mut(&area)
            .and_then(|map| map.get_mut(&(at, id)));
        Ok(m.map(|m| {
            m.labels = labels.to_vec();
            m.entry()
        }))
    }

    async fn sweep(&self, policy: &MailRetention, now: DateTime<Utc>) -> StoreResult<Vec<MailRef>> {
        let cutoff = chrono::Duration::from_std(policy.max_age)
            .ok()
            .and_then(|d| now.checked_sub_signed(d))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        let mut st = self.lock();
        let mut doomed = Vec::new();
        for map in st.areas.values() {
            let excess = map.len().saturating_sub(policy.max_messages.max(1));
            for (i, (at, id)) in map.keys().enumerate() {
                if i < excess || *at < cutoff {
                    doomed.push(*id);
                } else {
                    break;
                }
            }
        }
        Ok(doomed.into_iter().filter_map(|id| st.remove(id)).collect())
    }

    async fn mailbox_folders(
        &self,
        space: Option<&str>,
        address: &str,
    ) -> StoreResult<Vec<MailboxInfo>> {
        let st = self.lock();
        let Some(mb) = st
            .mailboxes
            .get(&(space.map(String::from), address.to_string()))
        else {
            return Ok(vec![]);
        };
        let mut v: Vec<MailboxInfo> = mb.folders.iter().map(|(n, f)| State::info(n, f)).collect();
        v.sort_by(|a, b| (a.folder != INBOX, &a.folder).cmp(&(b.folder != INBOX, &b.folder)));
        Ok(v)
    }

    async fn mailboxes(&self, spaces: &SpaceFilter) -> StoreResult<Vec<MailboxOwner>> {
        let st = self.lock();
        let mut v: Vec<MailboxOwner> = st
            .mailboxes
            .iter()
            .filter(|((a, _), _)| spaces.matches(a.as_deref()))
            .map(|((a, addr), mb)| MailboxOwner {
                space: a.clone(),
                address: addr.clone(),
                folders: mb.folders.keys().cloned().collect(),
                has_password: mb.password.is_some(),
            })
            .collect();
        v.sort_by(|a, b| (&a.space, &a.address).cmp(&(&b.space, &b.address)));
        Ok(v)
    }

    async fn mailbox_create(&self, key: &MailboxKey) -> StoreResult<bool> {
        let mut st = self.lock();
        if st.existing(key).is_some() {
            return Ok(false);
        }
        st.folder(&key.space, &key.address, &folder_name(&key.folder));
        Ok(true)
    }

    async fn mailbox_delete(&self, key: &MailboxKey) -> StoreResult<bool> {
        let name = folder_name(&key.folder);
        if name == INBOX {
            return Ok(false);
        }
        let mut st = self.lock();
        Ok(st
            .mailboxes
            .get_mut(&(key.space.clone(), key.address.clone()))
            .is_some_and(|m| m.folders.remove(&name).is_some()))
    }

    async fn mailbox_info(&self, key: &MailboxKey) -> StoreResult<Option<MailboxInfo>> {
        let mut st = self.lock();
        let name = folder_name(&key.folder);
        Ok(st.existing(key).map(|f| State::info(&name, f)))
    }

    async fn mailbox_entries(&self, key: &MailboxKey) -> StoreResult<Vec<MailboxEntry>> {
        let st = self.lock();
        let Some(f) = st
            .mailboxes
            .get(&(key.space.clone(), key.address.clone()))
            .and_then(|m| m.folders.get(&folder_name(&key.folder)))
        else {
            return Ok(vec![]);
        };
        Ok(f.entries
            .iter()
            .filter_map(|(uid, s)| st.entry(*uid, s))
            .collect())
    }

    async fn mailbox_store_flags(
        &self,
        key: &MailboxKey,
        uids: &[u32],
        op: FlagOp,
        flags: &[String],
    ) -> StoreResult<Vec<MailboxEntry>> {
        let mut st = self.lock();
        let given = flag_set(flags);
        let mut changed = BTreeSet::new();
        if let Some(f) = st.existing(key) {
            for uid in uids {
                if let Some(s) = f.entries.get_mut(uid) {
                    match op {
                        FlagOp::Add => s.flags.extend(given.iter().cloned()),
                        FlagOp::Remove => s.flags.retain(|x| !given.contains(x)),
                        FlagOp::Replace => s.flags = given.clone(),
                    }
                    changed.insert(*uid);
                }
            }
        }
        let st = &*st;
        let f = st
            .mailboxes
            .get(&(key.space.clone(), key.address.clone()))
            .and_then(|m| m.folders.get(&folder_name(&key.folder)));
        Ok(f.map(|f| {
            changed
                .iter()
                .filter_map(|uid| f.entries.get(uid).and_then(|s| st.entry(*uid, s)))
                .collect()
        })
        .unwrap_or_default())
    }

    async fn mailbox_copy(
        &self,
        key: &MailboxKey,
        uids: &[u32],
        target: &str,
    ) -> StoreResult<Option<Vec<(u32, u32)>>> {
        let mut st = self.lock();
        let target_key = MailboxKey {
            folder: folder_name(target),
            ..key.clone()
        };
        if st.existing(&target_key).is_none() {
            return Ok(None);
        }
        let wanted: BTreeSet<u32> = uids.iter().copied().collect();
        let picked: Vec<(u32, Uuid, BTreeSet<String>)> = match st.existing(key) {
            Some(f) => f
                .entries
                .iter()
                .filter(|(u, _)| wanted.contains(u))
                .map(|(u, s)| (*u, s.message_id, s.flags.clone()))
                .collect(),
            None => vec![],
        };
        let Some(t) = st.existing(&target_key) else {
            return Ok(None);
        };
        let mut pairs = Vec::with_capacity(picked.len());
        for (uid, message_id, flags) in picked {
            let new = t.uid_next;
            t.uid_next += 1;
            t.entries.insert(new, Slot { message_id, flags });
            pairs.push((uid, new));
        }
        Ok(Some(pairs))
    }

    async fn mailbox_expunge(
        &self,
        key: &MailboxKey,
        only: Option<&[u32]>,
    ) -> StoreResult<Vec<u32>> {
        let mut st = self.lock();
        let Some(f) = st.existing(key) else {
            return Ok(vec![]);
        };
        let gone: Vec<u32> = f
            .entries
            .iter()
            .filter(|(u, s)| s.flags.contains("\\Deleted") && only.is_none_or(|o| o.contains(u)))
            .map(|(u, _)| *u)
            .collect();
        for u in &gone {
            f.entries.remove(u);
        }
        Ok(gone)
    }

    async fn mailbox_remove(&self, key: &MailboxKey, uids: &[u32]) -> StoreResult<Vec<u32>> {
        let mut st = self.lock();
        let Some(f) = st.existing(key) else {
            return Ok(vec![]);
        };
        let mut gone: Vec<u32> = uids
            .iter()
            .copied()
            .filter(|u| f.entries.remove(u).is_some())
            .collect();
        gone.sort_unstable();
        Ok(gone)
    }

    async fn mailbox_password(
        &self,
        space: Option<&str>,
        address: &str,
    ) -> StoreResult<Option<String>> {
        let st = self.lock();
        Ok(st
            .mailboxes
            .get(&(space.map(String::from), address.to_string()))
            .and_then(|m| m.password.clone()))
    }

    async fn set_mailbox_password(
        &self,
        space: Option<&str>,
        address: &str,
        hash: Option<&str>,
    ) -> StoreResult<bool> {
        let mut st = self.lock();
        let area = space.map(String::from);
        match hash {
            Some(h) => {
                st.folder(&area, address, INBOX);
                if let Some(m) = st.mailboxes.get_mut(&(area, address.to_string())) {
                    m.password = Some(h.to_string());
                }
                Ok(true)
            }
            None => Ok(st
                .mailboxes
                .get_mut(&(area, address.to_string()))
                .and_then(|m| m.password.take())
                .is_some()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::conformance;

    #[tokio::test]
    async fn memory_store_passes_the_conformance_checks() {
        conformance::messages(&MemoryMailStore::new(1000)).await;
        conformance::mailboxes(&MemoryMailStore::new(1000)).await;
        conformance::retention(&MemoryMailStore::new(1000)).await;
    }

    #[tokio::test]
    async fn memory_store_caps_each_area_on_insert() {
        let s = MemoryMailStore::new(2);
        let ids = conformance::fill(&s, Some("capped"), 3).await;
        let page = s
            .list(&MailQuery {
                spaces: SpaceFilter::In(vec!["capped".into()]),
                oldest_first: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let got: Vec<Uuid> = page.items.iter().map(|e| e.id).collect();
        assert_eq!(got, ids[1..].to_vec());
        let inbox = MailboxKey::inbox(Some("capped"), "rcpt@example.test");
        let uids: Vec<u32> = s
            .mailbox_entries(&inbox)
            .await
            .unwrap()
            .iter()
            .map(|e| e.uid)
            .collect();
        assert_eq!(uids, vec![2, 3]);
    }
}
