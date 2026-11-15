//! Live updates: applies the changes the drive watcher reports (`crate::watch`) to a
//! [`VolumeIndex`] in place, so it stays current between scans without being rebuilt.
//!
//! A change only names a path. Each one is reconciled with what the disk holds at that path now,
//! so changes may arrive late, twice or out of order, including those made while the scan that
//! built the index was running: applying them again does nothing.
//!
//! Entries keep their ids. New entries get new slots, removed ones leave unused slots (parent
//! [`NONE`]) and their names stay in the arenas until the index is compacted. The four sort orders
//! are updated by taking the changed entries out and merging them back in, which costs a pass
//! over each order instead of a full sort.

use super::{
    ATTR_DIRECTORY, ATTR_REPARSE_POINT, Entry, NONE, VolumeBuilder, VolumeIndex, ancestors, directory_ranks, lower_key, par_sort_by,
    store_name, valid_name,
};
use std::cmp::Ordering;
use std::collections::HashMap;

/// What the file system reports about one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Windows file attributes (`FILE_ATTRIBUTE_*`).
    pub attrs: u32,
    /// File size (ignored for folders).
    pub size: u64,
    /// Last write time as a Windows FILETIME.
    pub mtime: i64,
}

impl Meta {
    fn is_dir(&self) -> bool {
        self.attrs & ATTR_DIRECTORY != 0
    }
}

/// What the disk holds at a path now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stat {
    Found(Meta),
    Missing,
    /// It could not be checked (no access, for example): the index is left as it is.
    Unknown,
}

/// The file system the changes happened on. Paths are relative to the drive root, with `\`
/// between components (`Users\me\notes.txt`); the empty path is the root.
pub trait Disk {
    fn stat(&self, path: &str) -> Stat;
    /// The items in folder `path` that can be read, with their names.
    fn list(&self, path: &str) -> Vec<(String, Meta)>;
}

/// One reported change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Something at this path was created, deleted or modified.
    Path(String),
    /// An item moved from one path to another on the same drive. Its contents moved with it.
    Renamed { from: String, to: String },
}

/// What [`VolumeIndex::apply`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// The index changed.
    pub changed: bool,
    /// Some change could not be stored (the drive ran out of entry ids or name space): only a new
    /// scan brings the index up to date.
    pub rescan: bool,
}

/// More changed entries than this re-sort the path order instead of merging into it.
const PATH_MERGE_LIMIT: usize = 10_000;
/// Unused slots and dead name bytes are reclaimed (the index rebuilt) once they make up half of
/// the index and are more than these.
const COMPACT_MIN_SLOTS: usize = if cfg!(test) { 8 } else { 1 << 16 };
const COMPACT_MIN_BYTES: usize = if cfg!(test) { 64 } else { 16 << 20 };

fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('\\').filter(|c| !c.is_empty())
}

/// The folder part and the last component of `path`; `None` for the root.
fn split_last(path: &str) -> Option<(String, &str)> {
    let parts: Vec<&str> = components(path).collect();
    let (name, parent) = parts.split_last()?;
    Some((parent.join("\\"), name))
}

fn walked_dir(attrs: u32) -> bool {
    attrs & ATTR_DIRECTORY != 0 && attrs & ATTR_REPARSE_POINT == 0
}

/// Folders and files are different kinds; so are folders whose contents are indexed below them
/// and links (junctions, mount points), whose contents are not.
fn kind(attrs: u32) -> (bool, bool) {
    (attrs & ATTR_DIRECTORY != 0, walked_dir(attrs))
}

/// Ids of entries a patch named (or moved) that the name order does not list under their current
/// name and parent yet, and the names that renamed entries are still listed under there.
#[derive(Default)]
struct Lookup {
    fresh: HashMap<(u32, String), u32>,
    listed_as: HashMap<u32, String>,
}

impl Lookup {
    /// The child of folder `parent` called `name` (compared as the index compares names).
    fn child(&self, v: &VolumeIndex, parent: u32, name: &str) -> Option<u32> {
        let key = lower_key(name);
        let key = key.as_ref();
        let is_it = |id: u32| v.entries[id as usize].parent == parent && v.lower_name(id) == key;
        if let Some(&id) = self.fresh.get(&(parent, key.to_owned()))
            && is_it(id)
        {
            return Some(id);
        }
        let listed = |id: u32| self.listed_as.get(&id).map_or_else(|| v.lower_name(id), String::as_str);
        let start = v.by_name.partition_point(|&id| listed(id) < key);
        v.by_name[start..].iter().copied().take_while(|&id| listed(id) == key).find(|&id| is_it(id))
    }

    fn find(&self, v: &VolumeIndex, path: &str) -> Option<u32> {
        components(path).try_fold(0, |dir, name| self.child(v, dir, name))
    }
}

impl VolumeIndex {
    /// Whether the index already matches the disk at every path in `changes`, so applying them
    /// would change nothing. Checked first because applying needs a writable copy of the index.
    pub fn is_current(&self, changes: &[Change], disk: &dyn Disk) -> bool {
        let lookup = Lookup::default();
        let current = |path: &str| {
            let found = lookup.find(self, path);
            match (found, disk.stat(path)) {
                (Some(0), _) | (_, Stat::Unknown) | (None, Stat::Missing) => true,
                (Some(id), Stat::Found(m)) => {
                    let e = self.entry(id);
                    let name = split_last(path).map_or("", |(_, n)| n);
                    kind(e.attrs) == kind(m.attrs)
                        && e.attrs == m.attrs
                        && e.mtime == m.mtime
                        && (e.is_dir() || e.size == m.size)
                        && self.name(id) == name
                }
                _ => false,
            }
        };
        changes.iter().all(|c| match c {
            Change::Path(p) => current(p),
            Change::Renamed { from, to } => lookup.find(self, from).is_none() && current(to),
        })
    }

    /// Brings the index in line with the disk at every path in `changes`.
    pub fn apply(&mut self, changes: &[Change], disk: &dyn Disk) -> Applied {
        let mut p = Patcher {
            v: self,
            disk,
            lookup: Lookup::default(),
            removed: Vec::new(),
            removed_dir: false,
            named: Vec::new(),
            sized: Vec::new(),
            dated: Vec::new(),
            dir_moved: false,
            deferred: Vec::new(),
            out_of_room: false,
            changed: false,
        };
        for change in changes {
            match change {
                Change::Path(path) => p.refresh(path),
                Change::Renamed { from, to } => p.rename(from, to),
            }
        }
        let applied = Applied { changed: p.changed, rescan: p.out_of_room };
        if p.changed {
            p.finish();
        }
        applied
    }
}

struct Patcher<'a> {
    v: &'a mut VolumeIndex,
    disk: &'a dyn Disk,
    lookup: Lookup,
    /// Entries this patch removed: their slots are unused now.
    removed: Vec<u32>,
    removed_dir: bool,
    /// Entries whose name or folder changed (new ones included): the name and path orders.
    named: Vec<u32>,
    /// Entries whose size changed: the size order.
    sized: Vec<u32>,
    /// Entries whose modification time changed: the date order.
    dated: Vec<u32>,
    /// A folder got another name or parent, so its whole subtree moves in the path order.
    dir_moved: bool,
    /// Paths whose folder was gone when they were checked; a later rename of that folder in the
    /// same batch checks them again at their new place.
    deferred: Vec<String>,
    out_of_room: bool,
    changed: bool,
}

impl Patcher<'_> {
    fn find(&self, path: &str) -> Option<u32> {
        self.lookup.find(self.v, path)
    }

    fn entry(&self, id: u32) -> Entry {
        self.v.entries[id as usize]
    }

    /// Makes the index agree with the disk at `path`.
    fn refresh(&mut self, path: &str) {
        let found = self.find(path);
        if found == Some(0) {
            return;
        }
        let stat = self.disk.stat(path);
        if stat == Stat::Missing
            && let Some((parent, _)) = split_last(path)
            && !matches!(self.disk.stat(&parent), Stat::Found(_))
        {
            // Its folder is gone too: it was moved along with it (a rename reported later), or
            // deleted with it. That folder's own change handles both; see `Patcher::rename`.
            self.deferred.push(path.to_owned());
            return;
        }
        match (found, stat) {
            (_, Stat::Unknown) | (None, Stat::Missing) => {}
            (Some(id), Stat::Missing) => self.remove(id),
            (Some(id), Stat::Found(m)) => {
                if kind(self.entry(id).attrs) == kind(m.attrs) {
                    let name = split_last(path).map_or("", |(_, n)| n);
                    self.update(id, name, m);
                } else {
                    self.remove(id);
                    self.add(path, m);
                }
            }
            (None, Stat::Found(m)) => self.add(path, m),
        }
    }

    /// The folder at `path`, added (with its contents) when the index does not have it yet.
    fn folder(&mut self, path: &str) -> Option<u32> {
        if let Some(id) = self.find(path) {
            return Some(id);
        }
        if matches!(self.disk.stat(path), Stat::Found(m) if m.is_dir()) {
            self.refresh(path);
        }
        self.find(path)
    }

    /// Adds the item at `path`, which the index does not have, with everything below it.
    fn add(&mut self, path: &str, m: Meta) {
        let Some((parent_path, name)) = split_last(path) else { return };
        if !valid_name(name) {
            return;
        }
        let known = self.find(&parent_path);
        let Some(parent) = known.or_else(|| self.folder(&parent_path)) else { return };
        if known.is_none() && self.find(path).is_some() {
            // Adding its folder listed it.
            return;
        }
        // The contents of links are not indexed below them (as the scans do).
        if !walked_dir(self.entry(parent).attrs) {
            return;
        }
        let Some(id) = self.attach(parent, name, m) else { return };
        if !walked_dir(m.attrs) {
            return;
        }
        let mut stack = vec![(id, path.to_owned())];
        while let Some((dir, dir_path)) = stack.pop() {
            for (child, cm) in self.disk.list(&dir_path) {
                if !valid_name(&child) {
                    continue;
                }
                let Some(cid) = self.attach(dir, &child, cm) else { return };
                if walked_dir(cm.attrs) {
                    stack.push((cid, format!("{dir_path}\\{child}")));
                }
            }
        }
    }

    /// Stores a new entry in folder `parent`.
    fn attach(&mut self, parent: u32, name: &str, m: Meta) -> Option<u32> {
        let id = self.v.entries.len();
        let stored = if id < NONE as usize { store_name(&mut self.v.names, &mut self.v.lower, u32::MAX as usize, name) } else { None };
        let Some(n) = stored else {
            self.out_of_room = true;
            return None;
        };
        let size = if m.is_dir() { 0 } else { m.size };
        self.v.entries.push(Entry {
            name_off: n.name_off,
            lower_off: n.lower_off,
            parent,
            name_len: n.name_len,
            lower_len: n.lower_len,
            attrs: m.attrs,
            size,
            mtime: m.mtime,
        });
        let id = id as u32;
        self.lookup.fresh.insert((parent, self.v.lower_name(id).to_owned()), id);
        self.named.push(id);
        self.sized.push(id);
        self.dated.push(id);
        self.grow(parent, size);
        self.changed = true;
        Some(id)
    }

    /// Removes an entry and (when it is a folder, in [`Patcher::finish`]) everything below it.
    fn remove(&mut self, id: u32) {
        let e = self.entry(id);
        if id == 0 || e.parent == NONE {
            return;
        }
        self.shrink(e.parent, e.size);
        self.removed_dir |= e.is_dir();
        self.v.dead_name_bytes += e.name_len as usize + e.lower_len as usize;
        self.v.entries[id as usize].parent = NONE;
        self.removed.push(id);
        self.changed = true;
    }

    /// Takes `m` and the name `name` (which can differ in case or be a long form) for entry `id`.
    fn update(&mut self, id: u32, name: &str, m: Meta) {
        if name != self.v.name(id) && valid_name(name) {
            self.set_name(id, name);
        }
        let e = self.entry(id);
        if e.attrs != m.attrs {
            self.v.entries[id as usize].attrs = m.attrs;
            self.changed = true;
        }
        if !e.is_dir() && e.size != m.size {
            if m.size > e.size {
                self.grow(e.parent, m.size - e.size);
            } else {
                self.shrink(e.parent, e.size - m.size);
            }
            self.v.entries[id as usize].size = m.size;
            self.sized.push(id);
            self.changed = true;
        }
        if e.mtime != m.mtime {
            self.v.entries[id as usize].mtime = m.mtime;
            self.dated.push(id);
            self.changed = true;
        }
    }

    fn set_name(&mut self, id: u32, name: &str) {
        let old = self.v.lower_name(id).to_owned();
        let Some(n) = store_name(&mut self.v.names, &mut self.v.lower, u32::MAX as usize, name) else {
            self.out_of_room = true;
            return;
        };
        let e = &mut self.v.entries[id as usize];
        self.v.dead_name_bytes += e.name_len as usize + e.lower_len as usize;
        (e.name_off, e.name_len, e.lower_off, e.lower_len) = (n.name_off, n.name_len, n.lower_off, n.lower_len);
        let (parent, is_dir) = (e.parent, e.is_dir());
        let new = self.v.lower_name(id).to_owned();
        if new != old {
            self.dir_moved |= is_dir;
            self.named.push(id);
        }
        self.lookup.fresh.insert((parent, new), id);
        self.lookup.listed_as.entry(id).or_insert(old);
        self.changed = true;
    }

    /// Handles an item that moved from `from` to `to`, keeping its entry (and subtree) when the
    /// index has it.
    fn rename(&mut self, from: &str, to: &str) {
        if !self.try_move(from, to) {
            self.refresh(from);
            self.refresh(to);
        } else if from.to_lowercase() != to.to_lowercase() {
            // A new item may have taken the old name since (unless only the case changed).
            self.refresh(from);
        }
        // Changes reported below the old path before the move happened below the new one.
        let prefix = format!("{}\\", from.to_lowercase());
        let depth = components(from).count();
        let (moved, rest): (Vec<String>, Vec<String>) =
            std::mem::take(&mut self.deferred).into_iter().partition(|d| d.to_lowercase().starts_with(&prefix));
        self.deferred = rest;
        for d in moved {
            let below: Vec<&str> = components(&d).skip(depth).collect();
            self.refresh(&format!("{to}\\{}", below.join("\\")));
        }
    }

    fn try_move(&mut self, from: &str, to: &str) -> bool {
        let Some(id) = self.find(from).filter(|&id| id != 0) else { return false };
        let Stat::Found(m) = self.disk.stat(to) else { return false };
        let Some((parent_path, name)) = split_last(to) else { return false };
        if !valid_name(name) || kind(self.entry(id).attrs) != kind(m.attrs) {
            return false;
        }
        // Moving onto an existing item (saving through a temporary file) replaces it.
        if let Some(old) = self.find(to)
            && old != id
        {
            self.remove(old);
        }
        let Some(parent) = self.folder(&parent_path) else { return false };
        // A folder cannot move into itself; nor can anything go below a link.
        if !walked_dir(self.entry(parent).attrs) || ancestors(&self.v.entries, parent).any(|a| a == id) || self.find(from) != Some(id) {
            return false;
        }
        let e = self.entry(id);
        if e.parent != parent {
            self.shrink(e.parent, e.size);
            self.v.entries[id as usize].parent = parent;
            self.grow(parent, e.size);
            self.lookup.fresh.insert((parent, self.v.lower_name(id).to_owned()), id);
            self.named.push(id);
            self.dir_moved |= e.is_dir();
            self.changed = true;
        }
        self.update(id, name, m);
        true
    }

    /// Adds `amount` to the sizes of `dir` and the folders above it.
    fn grow(&mut self, dir: u32, amount: u64) {
        self.resize(dir, |s| s.saturating_add(amount), amount);
    }

    fn shrink(&mut self, dir: u32, amount: u64) {
        self.resize(dir, |s| s.saturating_sub(amount), amount);
    }

    fn resize(&mut self, dir: u32, f: impl Fn(u64) -> u64, amount: u64) {
        if amount == 0 {
            return;
        }
        let mut cur = dir;
        for _ in 0..self.v.entries.len() {
            if cur == NONE {
                break;
            }
            let e = &mut self.v.entries[cur as usize];
            e.size = f(e.size);
            if cur != 0 {
                self.sized.push(cur);
            }
            cur = e.parent;
        }
    }

    /// Removes what was below removed folders and brings the sort orders up to date.
    fn finish(mut self) {
        if self.removed_dir {
            self.detach_orphans();
        }
        let v = &mut *self.v;
        let entries = &v.entries;
        let live = |ids: &mut Vec<u32>| {
            ids.retain(|&i| i != 0 && entries[i as usize].parent != NONE);
            ids.sort_unstable();
            ids.dedup();
        };
        live(&mut self.named);
        live(&mut self.sized);
        live(&mut self.dated);
        let any_removed = !self.removed.is_empty();

        let mut mark = vec![false; entries.len()];
        let lower = &v.lower;
        let lower_of = |i: u32| {
            let e = &entries[i as usize];
            lower.get(e.lower_off as usize..e.lower_off as usize + e.lower_len as usize).unwrap_or("")
        };
        let by_lower = |a: u32, b: u32| lower_of(a).cmp(lower_of(b)).then(a.cmp(&b));
        let by_size = |a: u32, b: u32| entries[a as usize].size.cmp(&entries[b as usize].size).then(a.cmp(&b));
        let by_date = |a: u32, b: u32| entries[a as usize].mtime.cmp(&entries[b as usize].mtime).then(a.cmp(&b));

        reorder(&mut v.by_name, &self.named, any_removed, entries, &mut mark, &by_lower);
        reorder(&mut v.by_size, &self.sized, any_removed, entries, &mut mark, &by_size);
        reorder(&mut v.by_date, &self.dated, any_removed, entries, &mut mark, &by_date);

        if self.dir_moved || self.named.len() > PATH_MERGE_LIMIT {
            // Folders changed place, so whole subtrees move: sort again, as a scan does.
            let rank = directory_ranks(entries, &lower_of);
            let mut by_path = v.by_name.clone();
            par_sort_by(&mut by_path, |&a, &b| {
                let (pa, pb) = (entries[a as usize].parent, entries[b as usize].parent);
                rank[pa as usize].cmp(&rank[pb as usize]).then_with(|| by_lower(a, b))
            });
            v.by_path = by_path;
        } else if !self.named.is_empty() || any_removed {
            // The folders kept their relative order, so comparing the chains of folder names
            // above two entries orders them as the folders' ranks would, without computing those.
            let chain = |dir: u32| -> Vec<u32> {
                let mut c: Vec<u32> = ancestors(entries, dir).collect();
                c.reverse();
                c
            };
            let by_chain = |ca: &[u32], cb: &[u32]| {
                ca.iter()
                    .zip(cb)
                    .find(|(x, y)| x != y)
                    .map_or_else(|| ca.len().cmp(&cb.len()), |(&x, &y)| by_lower(x, y))
            };
            let chains: HashMap<u32, Vec<u32>> = self.named.iter().map(|&i| (i, chain(entries[i as usize].parent))).collect();
            let mut added = self.named.clone();
            added.sort_unstable_by(|&a, &b| by_chain(&chains[&a], &chains[&b]).then_with(|| by_lower(a, b)));
            for &i in &self.named {
                mark[i as usize] = true;
            }
            let mut by_path = std::mem::take(&mut v.by_path);
            by_path.retain(|&i| !mark[i as usize] && entries[i as usize].parent != NONE);
            let mut out = Vec::with_capacity(by_path.len() + added.len());
            let mut start = 0;
            for &a in &added {
                let ca = &chains[&a];
                let pos = start
                    + by_path[start..].partition_point(|&b| {
                        let cb = chain(entries[b as usize].parent);
                        by_chain(&cb, ca).then_with(|| by_lower(b, a)) == Ordering::Less
                    });
                out.extend_from_slice(&by_path[start..pos]);
                out.push(a);
                start = pos;
            }
            out.extend_from_slice(&by_path[start..]);
            v.by_path = out;
        }

        let live_count = v.by_name.len();
        let dead_slots = v.entries.len() - 1 - live_count;
        let arena = v.names.len() + v.lower.len();
        if (dead_slots > COMPACT_MIN_SLOTS && dead_slots > live_count / 2) || (v.dead_name_bytes > COMPACT_MIN_BYTES && v.dead_name_bytes > arena / 2) {
            compact(v);
        }
    }

    /// Removes the entries below removed folders (they stay linked to the unused slots).
    fn detach_orphans(&mut self) {
        const UNKNOWN: u8 = 0;
        const ROOTED: u8 = 1;
        const DETACHED: u8 = 2;
        let entries = &mut self.v.entries;
        let n = entries.len();
        let mut state = vec![UNKNOWN; n];
        state[0] = ROOTED;
        let mut chain = Vec::new();
        for start in 1..n {
            let mut cur = start;
            while state[cur] == UNKNOWN && chain.len() <= n {
                let parent = entries[cur].parent;
                if parent == NONE {
                    state[cur] = DETACHED;
                    break;
                }
                chain.push(cur);
                cur = parent as usize;
            }
            let end = if state[cur] == UNKNOWN { DETACHED } else { state[cur] };
            for id in chain.drain(..) {
                state[id] = end;
            }
        }
        for (i, e) in entries.iter_mut().enumerate().skip(1) {
            if state[i] == DETACHED && e.parent != NONE {
                e.parent = NONE;
                self.v.dead_name_bytes += e.name_len as usize + e.lower_len as usize;
                self.removed.push(i as u32);
            }
        }
    }
}

/// Takes `touched` (and every removed entry) out of `order` and merges `touched` back in at the
/// places `cmp` gives them. `touched` holds distinct live ids.
fn reorder(order: &mut Vec<u32>, touched: &[u32], any_removed: bool, entries: &[Entry], mark: &mut [bool], cmp: &dyn Fn(u32, u32) -> Ordering) {
    if touched.is_empty() && !any_removed {
        return;
    }
    for &i in touched {
        mark[i as usize] = true;
    }
    let mut base = std::mem::take(order);
    base.retain(|&i| !mark[i as usize] && entries[i as usize].parent != NONE);
    for &i in touched {
        mark[i as usize] = false;
    }
    let mut added = touched.to_vec();
    added.sort_unstable_by(|&a, &b| cmp(a, b));
    let mut out = Vec::with_capacity(base.len() + added.len());
    let mut start = 0;
    for &a in &added {
        let pos = start + base[start..].partition_point(|&b| cmp(b, a) == Ordering::Less);
        out.extend_from_slice(&base[start..pos]);
        out.push(a);
        start = pos;
    }
    out.extend_from_slice(&base[start..]);
    *order = out;
}

/// Rebuilds the index without its unused slots and dead names. Ids change.
fn compact(v: &mut VolumeIndex) {
    let n = v.entries.len();
    let mut ids = vec![NONE; n];
    ids[0] = 0;
    let mut next = 1u32;
    for (i, id) in ids.iter_mut().enumerate().skip(1) {
        if v.entries[i].parent != NONE {
            *id = next;
            next += 1;
        }
    }
    let mut b = VolumeBuilder::new(v.letter);
    b.reserve(next as usize);
    for i in 1..n {
        if ids[i] == NONE {
            continue;
        }
        let e = v.entries[i];
        b.set(ids[i], v.name(i as u32), ids[e.parent as usize], e.attrs, if e.is_dir() { 0 } else { e.size }, e.mtime);
    }
    if b.check_capacity().is_err() {
        return;
    }
    let mut fresh = b.finish(v.source, v.build_time);
    fresh.built_at = v.built_at;
    *v = fresh;
}

#[cfg(test)]
mod tests {
    use super::super::{Source, check_orders, check_tree, compute_folder_sizes};
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Duration;

    /// An in-memory drive. Paths compare without case, like on Windows.
    #[derive(Default)]
    struct FakeDisk {
        items: BTreeMap<String, (String, Meta)>,
        denied: BTreeSet<String>,
    }

    fn file(size: u64, mtime: i64) -> Meta {
        Meta { attrs: 0x20, size, mtime }
    }

    fn dir() -> Meta {
        Meta { attrs: ATTR_DIRECTORY, size: 0, mtime: 1 }
    }

    impl FakeDisk {
        fn put(&mut self, path: &str, m: Meta) {
            self.items.insert(path.to_lowercase(), (path.to_owned(), m));
        }

        fn delete(&mut self, path: &str) {
            let key = path.to_lowercase();
            let below = format!("{key}\\");
            self.items.retain(|k, _| *k != key && !k.starts_with(&below));
        }

        /// Moves `from` (with everything below it) to `to`.
        fn rename(&mut self, from: &str, to: &str) {
            let key = from.to_lowercase();
            let below = format!("{key}\\");
            let moved: Vec<(String, Meta)> = self
                .items
                .iter()
                .filter(|(k, _)| **k == key || k.starts_with(&below))
                .map(|(_, (p, m))| (format!("{to}{}", &p[from.len()..]), *m))
                .collect();
            self.delete(to);
            self.delete(from);
            for (p, m) in moved {
                self.put(&p, m);
            }
        }

        /// The index a scan of this drive builds.
        fn scan(&self) -> VolumeIndex {
            let mut b = VolumeBuilder::new('T');
            let mut ids: HashMap<String, u32> = HashMap::from([(String::new(), 0)]);
            // Parents sort before their children.
            for (key, (path, m)) in &self.items {
                let (parent, name) = split_last(path).unwrap();
                let id = b.push(name, ids[&parent.to_lowercase()], m.attrs, m.size, m.mtime);
                ids.insert(key.clone(), id);
            }
            b.finish(Source::Walk, Duration::ZERO)
        }
    }

    impl Disk for FakeDisk {
        fn stat(&self, path: &str) -> Stat {
            if self.denied.contains(&path.to_lowercase()) {
                return Stat::Unknown;
            }
            if path.is_empty() {
                return Stat::Found(dir());
            }
            self.items.get(&path.to_lowercase()).map_or(Stat::Missing, |(_, m)| Stat::Found(*m))
        }

        fn list(&self, path: &str) -> Vec<(String, Meta)> {
            let key = path.to_lowercase();
            self.items
                .iter()
                .filter_map(|(k, (p, m))| {
                    let (parent, name) = split_last(p)?;
                    (parent.to_lowercase() == key && *k != key).then(|| (name.to_owned(), *m))
                })
                .collect()
        }
    }

    fn sample_disk() -> FakeDisk {
        let mut d = FakeDisk::default();
        d.put("docs", dir());
        d.put(r"docs\readme.md", file(100, 10));
        d.put(r"docs\old", dir());
        d.put(r"docs\old\a.txt", file(5, 11));
        d.put("src", dir());
        d.put(r"src\main.rs", file(300, 12));
        d.put("big.bin", file(1000, 5));
        d
    }

    /// Every path with what the index says about it.
    fn contents(v: &VolumeIndex) -> Vec<(String, u32, u64, i64)> {
        let mut all: Vec<_> = v.by_name.iter().map(|&i| (v.full_path(i), v.entry(i).attrs, v.entry(i).size, v.entry(i).mtime)).collect();
        all.sort();
        all
    }

    /// The patched index is valid (it would load from the cache) and holds what a new scan of
    /// the disk would.
    fn assert_matches(v: &VolumeIndex, disk: &FakeDisk) {
        check_tree(&v.entries).unwrap();
        check_orders(&v.entries, &v.lower, &v.by_name, &v.by_path, &v.by_size, &v.by_date).unwrap();
        let live = v.entries.iter().skip(1).filter(|e| e.parent != NONE).count();
        for order in [&v.by_name, &v.by_path, &v.by_size, &v.by_date] {
            assert_eq!(order.len(), live);
        }
        let mut sizes = v.entries.clone();
        compute_folder_sizes(&mut sizes);
        let in_use = |(i, e): &(usize, &Entry)| *i == 0 || e.parent != NONE;
        assert!(
            sizes.iter().enumerate().filter(in_use).all(|(i, e)| e.size == v.entries[i].size),
            "folder sizes are off"
        );
        assert_eq!(contents(v), contents(&disk.scan()));
    }

    fn apply(v: &mut VolumeIndex, disk: &FakeDisk, changes: &[Change]) -> Applied {
        let applied = v.apply(changes, disk);
        assert_matches(v, disk);
        applied
    }

    fn path(p: &str) -> Change {
        Change::Path(p.to_owned())
    }

    fn renamed(from: &str, to: &str) -> Change {
        Change::Renamed { from: from.to_owned(), to: to.to_owned() }
    }

    #[test]
    fn files_are_added_changed_and_removed() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.put(r"src\lib.rs", file(40, 20));
        assert!(apply(&mut v, &disk, &[path(r"src\lib.rs")]).changed);

        disk.put(r"docs\readme.md", file(250, 21));
        assert!(!v.is_current(&[path(r"docs\readme.md")], &disk));
        apply(&mut v, &disk, &[path(r"docs\readme.md")]);
        assert!(v.is_current(&[path(r"docs\readme.md")], &disk));

        disk.delete("big.bin");
        apply(&mut v, &disk, &[path("big.bin")]);
        assert_eq!(v.entry(0).size, 250 + 5 + 300 + 40);
    }

    #[test]
    fn changes_that_change_nothing_are_ignored() {
        let disk = sample_disk();
        let mut v = disk.scan();
        let changes = [path(r"docs\readme.md"), path("never-existed.txt"), path(r"no\such\folder"), path("")];
        assert!(v.is_current(&changes, &disk));
        assert!(!apply(&mut v, &disk, &changes).changed);
    }

    #[test]
    fn items_that_cannot_be_checked_are_left_alone() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.denied.insert("src".into());
        assert!(!v.apply(&[path("src")], &disk).changed);
        assert!(contents(&v).iter().any(|c| c.0 == r"T:\src\main.rs"));
    }

    /// A folder copied or extracted in may be reported as one change: its contents are listed.
    #[test]
    fn new_folders_are_added_with_their_contents() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.put("new", dir());
        disk.put(r"new\deep", dir());
        disk.put(r"new\deep\x.txt", file(7, 1));
        disk.put(r"new\y.txt", file(8, 1));
        apply(&mut v, &disk, &[path("new")]);
        // Reported again, one by one, as they usually are: nothing changes.
        assert!(!apply(&mut v, &disk, &[path(r"new\deep"), path(r"new\deep\x.txt"), path(r"new\y.txt")]).changed);
    }

    /// A change inside a folder the index does not have yet (its own change comes later).
    #[test]
    fn changes_below_unknown_folders_add_the_folders() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.put("a", dir());
        disk.put(r"a\b", dir());
        disk.put(r"a\b\c.txt", file(3, 1));
        disk.put(r"a\other.txt", file(4, 1));
        apply(&mut v, &disk, &[path(r"a\b\c.txt"), path("a")]);
    }

    #[test]
    fn removed_folders_take_their_contents_along() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.delete("docs");
        apply(&mut v, &disk, &[path("docs")]);
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn renamed_folders_keep_their_contents() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        let old = v.by_name.iter().copied().find(|&i| v.name(i) == "a.txt").unwrap();
        disk.rename(r"docs\old", "archive");
        apply(&mut v, &disk, &[renamed(r"docs\old", "archive")]);
        // Moved, not listed again: the same entry.
        assert_eq!(v.full_path(old), r"T:\archive\a.txt");

        disk.rename("src", "Code");
        apply(&mut v, &disk, &[renamed("src", "Code")]);
        assert!(contents(&v).iter().any(|c| c.0 == r"T:\Code\main.rs"));
    }

    #[test]
    fn case_only_renames_keep_the_new_case() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.rename("big.bin", "BIG.bin");
        apply(&mut v, &disk, &[renamed("big.bin", "BIG.bin")]);
        assert!(contents(&v).iter().any(|c| c.0 == r"T:\BIG.bin"));
    }

    /// Editors save by writing a temporary file and renaming it over the original.
    #[test]
    fn renaming_over_a_file_replaces_it() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.put(r"docs\readme.md~tmp", file(120, 30));
        apply(&mut v, &disk, &[path(r"docs\readme.md~tmp")]);
        disk.rename(r"docs\readme.md~tmp", r"docs\readme.md");
        apply(&mut v, &disk, &[renamed(r"docs\readme.md~tmp", r"docs\readme.md")]);
        assert_eq!(v.len(), 7);
    }

    /// A rename whose old path the index does not have, or that is reported in halves.
    #[test]
    fn renames_of_unknown_items_are_reconciled() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.put(r"src\gen", dir());
        disk.put(r"src\gen\out.rs", file(9, 2));
        apply(&mut v, &disk, &[renamed(r"tmp\xyz", r"src\gen")]);
        disk.rename(r"src\gen", "gen2");
        apply(&mut v, &disk, &[path(r"src\gen"), path("gen2")]);
    }

    /// A file changed just before its folder was renamed is reported at its old path, where it
    /// no longer is. It used to be removed there; it is checked again at the folder's new place.
    #[test]
    fn changes_before_a_folder_rename_follow_the_folder() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        let a = v.by_name.iter().copied().find(|&i| v.name(i) == "a.txt").unwrap();
        disk.put(r"docs\old\a.txt", file(55, 40));
        disk.put(r"docs\old\b.txt", file(6, 41));
        disk.rename(r"docs\old\b.txt", r"docs\old\c.txt");
        disk.rename(r"docs\old", "older");
        apply(
            &mut v,
            &disk,
            &[path(r"docs\old\a.txt"), path(r"docs\old\b.txt"), renamed(r"docs\old\b.txt", r"docs\old\c.txt"), renamed(r"docs\old", "older")],
        );
        assert_eq!(v.full_path(a), r"T:\older\a.txt");
        assert_eq!(v.entry(a).size, 55);
    }

    /// The same for a folder that was deleted: its own change removes what was below it.
    #[test]
    fn changes_before_a_folder_delete_go_with_it() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.delete(r"docs\old");
        apply(&mut v, &disk, &[path(r"docs\old\a.txt"), path(r"docs\old")]);
        // Reported without the folder's change (lost), the file stays until the next scan.
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.delete(r"docs\old");
        v.apply(&[path(r"docs\old\a.txt")], &disk);
        assert!(contents(&v).iter().any(|c| c.0 == r"T:\docs\old\a.txt"));
    }

    #[test]
    fn folders_cannot_move_into_themselves() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        // Not a real rename (the disk did not do it), so it is reconciled instead.
        apply(&mut v, &disk, &[renamed("docs", r"docs\old\docs")]);
        disk.rename("docs", r"src\docs");
        apply(&mut v, &disk, &[renamed("docs", r"src\docs")]);
    }

    #[test]
    fn files_and_folders_can_swap_kinds() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.delete("src");
        disk.put("src", file(1, 1));
        apply(&mut v, &disk, &[path("src")]);
        disk.delete("big.bin");
        disk.put("big.bin", dir());
        disk.put(r"big.bin\in.txt", file(2, 2));
        apply(&mut v, &disk, &[path("big.bin")]);
    }

    /// Many changes in one batch, some depending on each other, in an unhelpful order.
    #[test]
    fn mixed_batches_end_up_like_a_scan() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        disk.put("x", dir());
        disk.put(r"x\1.txt", file(1, 1));
        disk.rename(r"docs\readme.md", r"x\readme.md");
        disk.delete(r"docs\old");
        disk.put(r"src\main.rs", file(301, 99));
        disk.rename("x", "y");
        let changes = [
            path(r"x\1.txt"),
            renamed(r"docs\readme.md", r"x\readme.md"),
            path(r"docs\old\a.txt"),
            path(r"docs\old"),
            path(r"src\main.rs"),
            renamed("x", "y"),
            path("x"),
        ];
        apply(&mut v, &disk, &changes);
    }

    #[test]
    fn churn_is_compacted_and_still_caches() {
        let mut disk = sample_disk();
        let mut v = disk.scan();
        for round in 0..20 {
            let name = format!(r"src\tmp{round}.txt");
            disk.put(&name, file(round, round as i64));
            apply(&mut v, &disk, &[path(&name)]);
            disk.delete(&name);
            apply(&mut v, &disk, &[path(&name)]);
        }
        assert!(v.slots() < 20, "unused slots were not reclaimed: {}", v.slots());

        let dir = std::env::temp_dir().join(format!("filecraft-test-patch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file_path = dir.join("T.idx");
        disk.put(r"docs\new.txt", file(1, 1));
        disk.delete(r"docs\old");
        apply(&mut v, &disk, &[path(r"docs\new.txt"), path(r"docs\old")]);
        v.save(&file_path).unwrap();
        let loaded = VolumeIndex::load(&file_path, 'T').unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(contents(&loaded), contents(&v));
    }

    /// Path order merges are checked against a full sort on a wider tree.
    #[test]
    fn path_order_merges_match_a_full_sort() {
        let mut disk = FakeDisk::default();
        for d in ["a", "a b", "a-b", "ab", "b"] {
            disk.put(d, dir());
            disk.put(&format!(r"{d}\z"), dir());
            disk.put(&format!(r"{d}\z\f1"), file(1, 1));
        }
        let mut v = disk.scan();
        let mut changes = Vec::new();
        for d in ["a", "a b", "ab"] {
            for name in ["0", "m", "zz", "Z"] {
                let p = format!(r"{d}\z\{name}");
                disk.put(&p, file(2, 2));
                changes.push(path(&p));
            }
            let p = format!(r"{d}\new");
            disk.put(&p, dir());
            changes.push(path(&p));
        }
        apply(&mut v, &disk, &changes);
    }
}
