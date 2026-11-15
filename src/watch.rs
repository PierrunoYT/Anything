//! Follows the changes on a drive with `ReadDirectoryChangesW`, so its index can be patched
//! (`index::patch`) instead of going stale until the next scan. This needs no administrator
//! rights and works on every local file system, NTFS or not.

use crate::index::patch::{Change, Disk, Meta, Stat};
use crossbeam_channel::{Sender, TrySendError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

/// What the watcher sends.
#[derive(Debug)]
pub enum Event {
    /// Paths are relative to the watched folder.
    Changes(Vec<Change>),
    /// Changes were lost (too many at once, or the receiver fell behind): only a new scan brings
    /// the index up to date.
    Overflow,
    /// Watching stopped (the drive went away, for example).
    Failed(String),
}

/// Watches a folder and everything below it until dropped.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Most change batches waiting for the receiver; more are dropped and reported as an overflow.
const QUEUED_BATCHES: usize = 4096;

impl Watcher {
    /// Starts watching `root` (normally a verbatim drive root, `\\?\X:\`), sending what changes
    /// to `events`. Fails when the folder cannot be watched.
    pub fn start(root: &Path, events: Sender<Event>) -> std::io::Result<Watcher> {
        let dir = sys::open(root)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new().name(format!("watch {}", root.display())).spawn({
            let stop = Arc::clone(&stop);
            let root = root.to_path_buf();
            move || sys::run(dir, &root, &stop, &mut Outbox { events, lost: false })
        })?;
        Ok(Watcher { stop, thread: Some(thread) })
    }

    /// The channel to pass to [`Watcher::start`].
    pub fn channel() -> (Sender<Event>, crossbeam_channel::Receiver<Event>) {
        crossbeam_channel::bounded(QUEUED_BATCHES)
    }
}

/// Sends events without ever blocking the watcher: when the receiver falls behind, batches are
/// dropped and an [`Event::Overflow`] goes out once there is room again.
struct Outbox {
    events: Sender<Event>,
    lost: bool,
}

impl Outbox {
    /// Returns false once nobody receives any more.
    fn send(&mut self, event: Event) -> bool {
        if self.lost {
            match self.events.try_send(Event::Overflow) {
                Ok(()) => self.lost = false,
                Err(TrySendError::Full(_)) => return true,
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        match self.events.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.lost = true;
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

#[cfg(test)]
const FILE_ACTION_ADDED: u32 = 1;
const FILE_ACTION_RENAMED_OLD_NAME: u32 = 4;
const FILE_ACTION_RENAMED_NEW_NAME: u32 = 5;

/// Reads the `FILE_NOTIFY_INFORMATION` records in `buf` as (action, path) pairs. Stops at the
/// first record that does not fit.
fn records(buf: &[u8]) -> Vec<(u32, String)> {
    let u32_at = |at: usize| buf.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let mut out = Vec::new();
    let mut at = 0usize;
    while let (Some(next), Some(action), Some(len)) = (u32_at(at), u32_at(at + 4), u32_at(at + 8)) {
        let Some(name) = buf.get(at + 12..at + 12 + len as usize) else { break };
        let units: Vec<u16> = name.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        out.push((action, String::from_utf16_lossy(&units)));
        if next == 0 {
            break;
        }
        at += next as usize;
    }
    out
}

/// Turns records into changes: a rename's old and new name become one [`Change::Renamed`];
/// everything else, and half a rename, names a path to check.
fn changes(records: Vec<(u32, String)>) -> Vec<Change> {
    let mut out = Vec::new();
    let mut old: Option<String> = None;
    for (action, path) in records {
        match action {
            FILE_ACTION_RENAMED_OLD_NAME => {
                if let Some(o) = old.replace(path) {
                    out.push(Change::Path(o));
                }
            }
            FILE_ACTION_RENAMED_NEW_NAME => match old.take() {
                Some(from) => out.push(Change::Renamed { from, to: path }),
                None => out.push(Change::Path(path)),
            },
            _ => {
                out.extend(old.take().map(Change::Path));
                // Each change is checked against the disk as it is now, so the same path reported
                // again right away (added, then modified) needs checking only once.
                if !matches!(out.last(), Some(Change::Path(p)) if *p == path) {
                    out.push(Change::Path(path));
                }
            }
        }
    }
    out.extend(old.map(Change::Path));
    out
}

/// The disk the changes happened on, for [`crate::index::patch`].
pub struct LiveDisk {
    root: PathBuf,
}

impl LiveDisk {
    pub fn new(root: PathBuf) -> Self {
        LiveDisk { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() { self.root.clone() } else { self.root.join(rel) }
    }
}

fn meta(m: &std::fs::Metadata) -> Meta {
    let (attrs, size, mtime) = crate::walk::attributes(m);
    Meta { attrs, size, mtime }
}

impl Disk for LiveDisk {
    fn stat(&self, rel: &str) -> Stat {
        match std::fs::symlink_metadata(self.path(rel)) {
            Ok(m) => Stat::Found(meta(&m)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stat::Missing,
            // ERROR_INVALID_NAME, ERROR_DIRECTORY: a path through a file, or a name no item has.
            Err(e) if matches!(e.raw_os_error(), Some(123 | 267)) => Stat::Missing,
            Err(_) => Stat::Unknown,
        }
    }

    fn list(&self, rel: &str) -> Vec<(String, Meta)> {
        let Ok(rd) = std::fs::read_dir(self.path(rel)) else { return Vec::new() };
        rd.flatten()
            .filter_map(|e| {
                // Names that are not valid Unicode are left out, as the scans do with their contents.
                let name = e.file_name().into_string().ok()?;
                Some((name, meta(&e.metadata().ok()?)))
            })
            .collect()
    }
}

#[cfg(windows)]
mod sys {
    use super::{Event, Outbox, changes, records};
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::Foundation::{ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION,
        FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetLongPathNameW, OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

    /// Size of the buffer the changes are read into. Changes that do not fit before it is read
    /// again are lost (and reported as an overflow).
    const BUFFER_BYTES: usize = 512 << 10;
    /// How often a waiting watcher checks whether it should stop, in milliseconds.
    const STOP_CHECK_MS: u32 = 250;

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    pub fn open(root: &Path) -> io::Result<OwnedHandle> {
        let name = wide(root);
        // SAFETY: `name` is NUL-terminated and outlives the call; the other arguments are flags
        // and null pointers the function accepts.
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `h` is a valid handle that nothing else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    /// Replaces 8.3 short names (`PROGRA~1`) in `rel`, which notifications sometimes carry, by
    /// the long names the index holds. Paths that no longer exist keep their last component.
    fn long_names(root: &Path, rel: String) -> String {
        if !rel.contains('~') {
            return rel;
        }
        let long = |rel: &str| -> Option<String> {
            let full = wide(&root.join(rel));
            let mut buf = vec![0u16; 1024];
            loop {
                // SAFETY: `full` is NUL-terminated and `buf` holds `buf.len()` units.
                let n = unsafe { GetLongPathNameW(full.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
                if n == 0 || n > 1 << 16 {
                    return None;
                }
                if n < buf.len() {
                    buf.truncate(n);
                    break;
                }
                buf.resize(n, 0);
            }
            let long = std::ffi::OsString::from_wide(&buf).into_string().ok()?;
            let prefix = root.to_str()?;
            let rest = long.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| &long[prefix.len()..])?;
            Some(rest.trim_start_matches('\\').to_owned())
        };
        if let Some(l) = long(&rel) {
            return l;
        }
        match rel.rsplit_once('\\') {
            Some((parent, name)) => match long(parent) {
                Some(p) if !p.is_empty() => format!("{p}\\{name}"),
                _ => rel,
            },
            None => rel,
        }
    }

    pub fn run(dir: OwnedHandle, root: &Path, stop: &AtomicBool, out: &mut Outbox) {
        let h = dir.as_raw_handle();
        // SAFETY: null attributes and name are allowed; a manual-reset event, initially unset.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            out.send(Event::Failed(io::Error::last_os_error().to_string()));
            return;
        }
        // SAFETY: `event` is a valid handle that nothing else owns.
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        // `u64`s, so the buffer is DWORD-aligned as the function requires.
        let mut buf = vec![0u64; BUFFER_BYTES / 8];
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_ATTRIBUTES
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE
            | FILE_NOTIFY_CHANGE_CREATION;
        loop {
            // SAFETY: an all-zero OVERLAPPED is valid.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = event.as_raw_handle();
            // SAFETY: `event` is a valid event handle.
            unsafe { ResetEvent(ov.hEvent) };
            // SAFETY: `buf` and `ov` stay alive and unmoved until the read completes: every path
            // below waits for completion (or cancels and then waits) before they are touched.
            let started = unsafe {
                ReadDirectoryChangesW(h, buf.as_mut_ptr().cast(), BUFFER_BYTES as u32, 1, filter, std::ptr::null_mut(), &mut ov, None)
            };
            if started == 0 {
                out.send(Event::Failed(io::Error::last_os_error().to_string()));
                return;
            }
            let mut got = 0u32;
            loop {
                // SAFETY: `ov.hEvent` is a valid event handle.
                if unsafe { WaitForSingleObject(ov.hEvent, STOP_CHECK_MS) } == WAIT_OBJECT_0 {
                    break;
                }
                if stop.load(Ordering::Relaxed) {
                    // SAFETY: cancels the read started above, then waits until it has finished
                    // with `buf` and `ov`.
                    unsafe {
                        CancelIoEx(h, &ov);
                        GetOverlappedResult(h, &ov, &mut got, 1);
                    }
                    return;
                }
            }
            // SAFETY: the read has completed (its event is set).
            let ok = unsafe { GetOverlappedResult(h, &ov, &mut got, 0) };
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if ok == 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error().map(|c| c as u32) {
                    Some(ERROR_NOTIFY_ENUM_DIR) => {
                        if !out.send(Event::Overflow) {
                            return;
                        }
                        continue;
                    }
                    Some(ERROR_OPERATION_ABORTED) => return,
                    _ => {
                        out.send(Event::Failed(err.to_string()));
                        return;
                    }
                }
            }
            if got == 0 {
                // The changes did not fit in the buffer.
                if !out.send(Event::Overflow) {
                    return;
                }
                continue;
            }
            // SAFETY: the first `got` bytes of `buf` (at most its size) were written by the read;
            // viewing initialized `u64`s as bytes is always valid.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), (got as usize).min(BUFFER_BYTES)) };
            let list: Vec<_> = records(bytes).into_iter().map(|(action, path)| (action, long_names(root, path))).collect();
            let found = changes(list);
            if !found.is_empty() && !out.send(Event::Changes(found)) {
                return;
            }
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use super::Outbox;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    pub struct Unsupported;

    pub fn open(_root: &Path) -> std::io::Result<Unsupported> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "following changes is only supported on Windows"))
    }

    pub fn run(_dir: Unsupported, _root: &Path, _stop: &AtomicBool, _out: &mut Outbox) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut r = Vec::new();
        r.extend_from_slice(&next.to_le_bytes());
        r.extend_from_slice(&action.to_le_bytes());
        r.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            r.extend_from_slice(&u.to_le_bytes());
        }
        while r.len() % 4 != 0 {
            r.push(0);
        }
        r
    }

    fn buffer(items: &[(u32, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (k, (action, name)) in items.iter().enumerate() {
            let last = k + 1 == items.len();
            let len = record(0, *action, name).len() as u32;
            buf.extend(record(if last { 0 } else { len }, *action, name));
        }
        buf
    }

    #[test]
    fn notification_records_are_read() {
        let buf = buffer(&[(1, "a.txt"), (3, r"dir\é.txt"), (2, "x")]);
        assert_eq!(records(&buf), [(1, "a.txt".into()), (3, r"dir\é.txt".into()), (2, "x".into())]);
        // A record cut short ends the list instead of reading past the buffer.
        assert_eq!(records(&buf[..buf.len() - 3]).len(), 2);
        assert!(records(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn renames_are_paired() {
        let p = |s: &str| Change::Path(s.into());
        let got = changes(vec![
            (FILE_ACTION_RENAMED_OLD_NAME, "old".into()),
            (FILE_ACTION_RENAMED_NEW_NAME, "new".into()),
            (FILE_ACTION_RENAMED_OLD_NAME, "half".into()),
            (FILE_ACTION_ADDED, "f".into()),
            (3, "f".into()),
            (3, "f".into()),
            (FILE_ACTION_RENAMED_NEW_NAME, "lonely".into()),
            (FILE_ACTION_RENAMED_OLD_NAME, "end".into()),
        ]);
        assert_eq!(got, [Change::Renamed { from: "old".into(), to: "new".into() }, p("half"), p("f"), p("lonely"), p("end")]);
    }

    #[test]
    fn a_full_receiver_turns_into_an_overflow() {
        let one = |p: &str| Event::Changes(vec![Change::Path(p.into())]);
        let is = |e: Result<Event, _>, p: &str| matches!(e, Ok(Event::Changes(c)) if c == [Change::Path(p.into())]);
        let (tx, rx) = crossbeam_channel::bounded(2);
        let mut out = Outbox { events: tx, lost: false };
        assert!(out.send(one("a")) && out.send(one("b")));
        assert!(out.send(one("dropped")));
        assert!(is(rx.try_recv(), "a") && is(rx.try_recv(), "b"));
        assert!(out.send(one("c")));
        assert!(matches!(rx.try_recv(), Ok(Event::Overflow)));
        assert!(is(rx.try_recv(), "c"));
        assert!(rx.try_recv().is_err());
        drop(rx);
        assert!(!out.send(Event::Overflow));
    }

    /// Changes made on disk reach the watcher, and patching a scanned index with them gives what
    /// a new scan gives.
    #[cfg(windows)]
    #[test]
    fn watched_changes_patch_a_scanned_index() {
        use std::sync::atomic::AtomicUsize;
        use std::time::{Duration, Instant};
        let plain = std::env::temp_dir().join(format!("filecraft-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&plain);
        std::fs::create_dir_all(plain.join("keep")).unwrap();
        std::fs::write(plain.join("keep").join("old.txt"), "old").unwrap();
        let root = crate::util::fs_path(&plain.to_string_lossy());
        let scan = || {
            let zero = || AtomicUsize::new(0);
            crate::walk::walk_volume('T', &root, &zero(), &zero(), &zero(), &AtomicBool::new(false)).unwrap()
        };
        let before = scan();

        let (tx, rx) = Watcher::channel();
        let watcher = Watcher::start(&root, tx).unwrap();
        std::fs::create_dir_all(plain.join("new").join("deep")).unwrap();
        std::fs::write(plain.join("new").join("deep").join("a.txt"), "hello").unwrap();
        std::fs::write(plain.join("keep").join("old.txt"), "longer now").unwrap();
        std::fs::rename(plain.join("keep"), plain.join("kept")).unwrap();

        let disk = LiveDisk::new(root.clone());
        let deadline = Instant::now() + Duration::from_secs(10);
        let all = |v: &crate::index::VolumeIndex| {
            let mut c: Vec<(String, u64)> = v.by_name.iter().map(|&i| (v.full_path(i), v.entry(i).size)).collect();
            c.sort();
            c
        };
        let expected = all(&scan());
        // Everything reported so far, applied as one batch (as the indexer batches them).
        let mut reported = Vec::new();
        let mut v = before.clone();
        while all(&v) != expected && Instant::now() < deadline {
            if let Ok(Event::Changes(c)) = rx.recv_timeout(Duration::from_millis(200)) {
                reported.extend(c);
                v = before.clone();
                v.apply(&reported, &disk);
            }
        }
        drop(watcher);
        let _ = std::fs::remove_dir_all(&plain);
        assert_eq!(all(&v), expected);
    }
}
