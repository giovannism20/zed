mod char_bag;
mod fuzzy;

use crate::{
    editor::{History, Snapshot as BufferSnapshot},
    sum_tree::{self, Edit, SumTree},
};
use anyhow::{anyhow, Result};
pub use fuzzy::match_paths;
use fuzzy::PathEntry;
use gpui::{scoped_pool, AppContext, Entity, ModelContext, ModelHandle, Task};
use ignore::dir::{Ignore, IgnoreBuilder};
use parking_lot::Mutex;
use smol::{channel::Sender, Timer};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
};
use std::{
    ffi::OsStr,
    fmt, fs,
    io::{self, Read, Write},
    ops::AddAssign,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub use fuzzy::PathMatch;

#[derive(Debug)]
enum ScanState {
    Idle,
    Scanning,
    Err(io::Error),
}

pub struct Worktree {
    snapshot: Snapshot,
    scanner: Arc<BackgroundScanner>,
    scan_state: ScanState,
    poll_scheduled: bool,
}

#[derive(Clone)]
pub struct Snapshot {
    id: usize,
    path: Arc<Path>,
    root_inode: Option<u64>,
    entries: SumTree<Entry>,
}

#[derive(Clone)]
pub struct FileHandle {
    worktree: ModelHandle<Worktree>,
    inode: u64,
}

impl Worktree {
    pub fn new(path: impl Into<Arc<Path>>, ctx: &mut ModelContext<Self>) -> Self {
        let scan_state = smol::channel::unbounded();
        let snapshot = Snapshot {
            id: ctx.model_id(),
            path: path.into(),
            root_inode: None,
            entries: Default::default(),
        };
        let scanner = Arc::new(BackgroundScanner::new(snapshot.clone(), scan_state.0));
        let tree = Self {
            snapshot,
            scanner,
            scan_state: ScanState::Idle,
            poll_scheduled: false,
        };

        let scanner = tree.scanner.clone();
        std::thread::spawn(move || scanner.run());

        ctx.spawn_stream(scan_state.1, Self::observe_scan_state, |_, _| {})
            .detach();

        tree
    }

    fn observe_scan_state(&mut self, scan_state: ScanState, ctx: &mut ModelContext<Self>) {
        self.scan_state = scan_state;
        self.poll_entries(ctx);
    }

    fn poll_entries(&mut self, ctx: &mut ModelContext<Self>) {
        self.snapshot = self.scanner.snapshot();
        ctx.notify();

        if self.is_scanning() && !self.poll_scheduled {
            ctx.spawn(Timer::after(Duration::from_millis(100)), |this, _, ctx| {
                this.poll_scheduled = false;
                this.poll_entries(ctx);
            })
            .detach();
            self.poll_scheduled = true;
        }
    }

    fn is_scanning(&self) -> bool {
        if let ScanState::Scanning = self.scan_state {
            true
        } else {
            false
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    pub fn contains_path(&self, path: &Path) -> bool {
        path.starts_with(&self.snapshot.path)
    }

    pub fn has_inode(&self, inode: u64) -> bool {
        self.snapshot.entries.get(&inode).is_some()
    }

    pub fn file_count(&self) -> usize {
        self.snapshot.entries.summary().file_count
    }

    pub fn abs_path_for_inode(&self, ino: u64) -> Result<PathBuf> {
        let mut result = self.snapshot.path.to_path_buf();
        result.push(self.path_for_inode(ino, false)?);
        Ok(result)
    }

    pub fn path_for_inode(&self, ino: u64, include_root: bool) -> Result<PathBuf> {
        let mut components = Vec::new();
        let mut entry = self
            .snapshot
            .entries
            .get(&ino)
            .ok_or_else(|| anyhow!("entry does not exist in worktree"))?;
        components.push(entry.name());
        while let Some(parent) = entry.parent() {
            entry = self.snapshot.entries.get(&parent).unwrap();
            components.push(entry.name());
        }

        let mut components = components.into_iter().rev();
        if !include_root {
            components.next();
        }

        let mut path = PathBuf::new();
        for component in components {
            path.push(component);
        }
        Ok(path)
    }

    pub fn load_history(
        &self,
        ino: u64,
        ctx: &AppContext,
    ) -> impl Future<Output = Result<History>> {
        let path = self.abs_path_for_inode(ino);
        ctx.background_executor().spawn(async move {
            let mut file = std::fs::File::open(&path?)?;
            let mut base_text = String::new();
            file.read_to_string(&mut base_text)?;
            Ok(History::new(Arc::from(base_text)))
        })
    }

    pub fn save<'a>(
        &self,
        ino: u64,
        content: BufferSnapshot,
        ctx: &AppContext,
    ) -> Task<Result<()>> {
        let path = self.abs_path_for_inode(ino);
        eprintln!("save to path: {:?}", path);
        ctx.background_executor().spawn(async move {
            let buffer_size = content.text_summary().bytes.min(10 * 1024);
            let file = std::fs::File::create(&path?)?;
            let mut writer = std::io::BufWriter::with_capacity(buffer_size, file);
            for chunk in content.fragments() {
                writer.write(chunk.as_bytes())?;
            }
            writer.flush()?;
            Ok(())
        })
    }

    fn fmt_entry(&self, f: &mut fmt::Formatter<'_>, ino: u64, indent: usize) -> fmt::Result {
        match self.snapshot.entries.get(&ino).unwrap() {
            Entry::Dir { name, children, .. } => {
                write!(
                    f,
                    "{}{}/ ({})\n",
                    " ".repeat(indent),
                    name.to_string_lossy(),
                    ino
                )?;
                for child_id in children.iter() {
                    self.fmt_entry(f, *child_id, indent + 2)?;
                }
                Ok(())
            }
            Entry::File { name, .. } => write!(
                f,
                "{}{} ({})\n",
                " ".repeat(indent),
                name.to_string_lossy(),
                ino
            ),
        }
    }

    #[cfg(test)]
    pub fn files<'a>(&'a self) -> impl Iterator<Item = u64> + 'a {
        self.snapshot
            .entries
            .cursor::<(), ()>()
            .filter_map(|entry| {
                if let Entry::File { inode, .. } = entry {
                    Some(*inode)
                } else {
                    None
                }
            })
    }
}

impl Entity for Worktree {
    type Event = ();
}

impl fmt::Debug for Worktree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(root_ino) = self.snapshot.root_inode {
            self.fmt_entry(f, root_ino, 0)
        } else {
            write!(f, "Empty tree\n")
        }
    }
}

impl Snapshot {
    pub fn file_count(&self) -> usize {
        self.entries.summary().file_count
    }

    pub fn root_entry(&self) -> Option<&Entry> {
        self.root_inode.and_then(|inode| self.entries.get(&inode))
    }

    fn inode_for_path(&self, path: impl AsRef<Path>) -> Option<u64> {
        let path = path.as_ref();
        self.root_inode.and_then(|mut inode| {
            'components: for path_component in path {
                if let Some(Entry::Dir { children, .. }) = &self.entries.get(&inode) {
                    for child in children.as_ref() {
                        if self.entries.get(child).map(|entry| entry.name()) == Some(path_component)
                        {
                            inode = *child;
                            continue 'components;
                        }
                    }
                }
                return None;
            }
            Some(inode)
        })
    }

    fn entry_for_path(&self, path: impl AsRef<Path>) -> Option<&Entry> {
        self.inode_for_path(path)
            .and_then(|inode| self.entries.get(&inode))
    }
}

impl FileHandle {
    pub fn path(&self, ctx: &AppContext) -> PathBuf {
        self.worktree
            .read(ctx)
            .path_for_inode(self.inode, false)
            .unwrap()
    }

    pub fn load_history(&self, ctx: &AppContext) -> impl Future<Output = Result<History>> {
        self.worktree.read(ctx).load_history(self.inode, ctx)
    }

    pub fn save<'a>(&self, content: BufferSnapshot, ctx: &AppContext) -> Task<Result<()>> {
        let worktree = self.worktree.read(ctx);
        worktree.save(self.inode, content, ctx)
    }

    pub fn entry_id(&self) -> (usize, u64) {
        (self.worktree.id(), self.inode)
    }
}

#[derive(Clone, Debug)]
pub enum Entry {
    Dir {
        parent: Option<u64>,
        name: Arc<OsStr>,
        inode: u64,
        is_symlink: bool,
        is_ignored: bool,
        children: Arc<[u64]>,
        pending: bool,
    },
    File {
        parent: Option<u64>,
        name: Arc<OsStr>,
        path: PathEntry,
        inode: u64,
        is_symlink: bool,
        is_ignored: bool,
    },
}

impl Entry {
    fn ino(&self) -> u64 {
        match self {
            Entry::Dir { inode: ino, .. } => *ino,
            Entry::File { inode: ino, .. } => *ino,
        }
    }

    fn parent(&self) -> Option<u64> {
        match self {
            Entry::Dir { parent, .. } => *parent,
            Entry::File { parent, .. } => *parent,
        }
    }

    fn name(&self) -> &OsStr {
        match self {
            Entry::Dir { name, .. } => name,
            Entry::File { name, .. } => name,
        }
    }
}

impl sum_tree::Item for Entry {
    type Summary = EntrySummary;

    fn summary(&self) -> Self::Summary {
        EntrySummary {
            max_ino: self.ino(),
            file_count: if matches!(self, Self::File { .. }) {
                1
            } else {
                0
            },
        }
    }
}

impl sum_tree::KeyedItem for Entry {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.ino()
    }
}

#[derive(Clone, Debug, Default)]
pub struct EntrySummary {
    max_ino: u64,
    file_count: usize,
}

impl<'a> AddAssign<&'a EntrySummary> for EntrySummary {
    fn add_assign(&mut self, rhs: &'a EntrySummary) {
        self.max_ino = rhs.max_ino;
        self.file_count += rhs.file_count;
    }
}

impl<'a> sum_tree::Dimension<'a, EntrySummary> for u64 {
    fn add_summary(&mut self, summary: &'a EntrySummary) {
        *self = summary.max_ino;
    }
}

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct FileCount(usize);

impl<'a> sum_tree::Dimension<'a, EntrySummary> for FileCount {
    fn add_summary(&mut self, summary: &'a EntrySummary) {
        self.0 += summary.file_count;
    }
}

struct BackgroundScanner {
    snapshot: Mutex<Snapshot>,
    notify: Sender<ScanState>,
    thread_pool: scoped_pool::Pool,
}

impl BackgroundScanner {
    fn new(snapshot: Snapshot, notify: Sender<ScanState>) -> Self {
        Self {
            snapshot: Mutex::new(snapshot),
            notify,
            thread_pool: scoped_pool::Pool::new(16),
        }
    }

    fn path(&self) -> Arc<Path> {
        self.snapshot.lock().path.clone()
    }

    fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().clone()
    }

    fn run(&self) {
        let path = {
            let mut snapshot = self.snapshot.lock();
            let canonical_path = snapshot
                .path
                .canonicalize()
                .map(Arc::from)
                .unwrap_or_else(|_| snapshot.path.clone());
            snapshot.path = canonical_path.clone();
            canonical_path
        };

        // Create the event stream before we start scanning to ensure we receive events for changes
        // that occur in the middle of the scan.
        let event_stream =
            fsevent::EventStream::new(&[path.as_ref()], Duration::from_millis(100), |events| {
                if smol::block_on(self.notify.send(ScanState::Scanning)).is_err() {
                    return false;
                }

                if let Err(error) = self.process_events(events) {
                    log::error!("error handling events: {}", error);
                    return false;
                }

                if smol::block_on(self.notify.send(ScanState::Idle)).is_err() {
                    return false;
                }

                true
            });

        if smol::block_on(self.notify.send(ScanState::Scanning)).is_err() {
            return;
        }

        if let Err(err) = self.scan_dirs() {
            if smol::block_on(self.notify.send(ScanState::Err(err))).is_err() {
                return;
            }
        }

        if smol::block_on(self.notify.send(ScanState::Idle)).is_err() {
            return;
        }

        event_stream.run();
    }

    fn scan_dirs(&self) -> io::Result<()> {
        let path = self.path();
        let metadata = fs::metadata(&path)?;
        let inode = metadata.ino();
        let is_symlink = fs::symlink_metadata(&path)?.file_type().is_symlink();
        let name = Arc::from(path.file_name().unwrap_or(OsStr::new("/")));
        let relative_path = PathBuf::from(&name);

        let mut ignore = IgnoreBuilder::new().build().add_parents(&path).unwrap();
        if metadata.is_dir() {
            ignore = ignore.add_child(&path).unwrap();
        }
        let is_ignored = ignore.matched(&path, metadata.is_dir()).is_ignore();

        if metadata.file_type().is_dir() {
            let is_ignored = is_ignored || name.as_ref() == ".git";
            let dir_entry = Entry::Dir {
                parent: None,
                name,
                inode,
                is_symlink,
                is_ignored,
                children: Arc::from([]),
                pending: true,
            };
            self.insert_entries(Some(dir_entry.clone()));
            self.snapshot.lock().root_inode = Some(inode);

            let (tx, rx) = crossbeam_channel::unbounded();

            tx.send(Ok(ScanJob {
                ino: inode,
                path: path.clone(),
                relative_path,
                dir_entry,
                ignore: Some(ignore),
                scan_queue: tx.clone(),
            }))
            .unwrap();
            drop(tx);

            let mut results = Vec::new();
            results.resize_with(self.thread_pool.workers(), || Ok(()));
            self.thread_pool.scoped(|pool| {
                for result in &mut results {
                    pool.execute(|| {
                        let result = result;
                        while let Ok(job) = rx.recv() {
                            if let Err(err) = job.and_then(|job| self.scan_dir(job, None)) {
                                *result = Err(err);
                                break;
                            }
                        }
                    });
                }
            });
            results.into_iter().collect::<io::Result<()>>()?;
        } else {
            self.insert_entries(Some(Entry::File {
                parent: None,
                name,
                path: PathEntry::new(inode, &relative_path, is_ignored),
                inode,
                is_symlink,
                is_ignored,
            }));
            self.snapshot.lock().root_inode = Some(inode);
        }

        Ok(())
    }

    fn scan_dir(&self, job: ScanJob, mut children: Option<&mut Vec<u64>>) -> io::Result<()> {
        let scan_queue = job.scan_queue;
        let mut dir_entry = job.dir_entry;

        let mut new_children = Vec::new();
        let mut new_entries = Vec::new();
        let mut new_jobs = Vec::new();

        for child_entry in fs::read_dir(&job.path)? {
            let child_entry = child_entry?;
            let name: Arc<OsStr> = child_entry.file_name().into();
            let relative_path = job.relative_path.join(name.as_ref());
            let metadata = child_entry.metadata()?;
            let ino = metadata.ino();
            let is_symlink = metadata.file_type().is_symlink();
            let path = job.path.join(name.as_ref());

            new_children.push(ino);
            if let Some(children) = children.as_mut() {
                children.push(ino);
            }
            if metadata.is_dir() {
                let mut is_ignored = true;
                let mut ignore = None;

                if let Some(parent_ignore) = job.ignore.as_ref() {
                    let child_ignore = parent_ignore.add_child(&path).unwrap();
                    is_ignored =
                        child_ignore.matched(&path, true).is_ignore() || name.as_ref() == ".git";
                    if !is_ignored {
                        ignore = Some(child_ignore);
                    }
                }

                let dir_entry = Entry::Dir {
                    parent: Some(job.ino),
                    name,
                    inode: ino,
                    is_symlink,
                    is_ignored,
                    children: Arc::from([]),
                    pending: true,
                };
                new_entries.push(dir_entry.clone());
                new_jobs.push(ScanJob {
                    ino,
                    path: Arc::from(path),
                    relative_path,
                    dir_entry,
                    ignore,
                    scan_queue: scan_queue.clone(),
                });
            } else {
                let is_ignored = job
                    .ignore
                    .as_ref()
                    .map_or(true, |i| i.matched(&path, false).is_ignore());
                new_entries.push(Entry::File {
                    parent: Some(job.ino),
                    name,
                    path: PathEntry::new(ino, &relative_path, is_ignored),
                    inode: ino,
                    is_symlink,
                    is_ignored,
                });
            };
        }

        if let Entry::Dir {
            children, pending, ..
        } = &mut dir_entry
        {
            *children = Arc::from(new_children);
            *pending = false;
        } else {
            unreachable!()
        }
        new_entries.push(dir_entry);

        self.insert_entries(new_entries);
        for new_job in new_jobs {
            scan_queue.send(Ok(new_job)).unwrap();
        }

        Ok(())
    }

    fn process_events(&self, mut events: Vec<fsevent::Event>) -> Result<()> {
        let snapshot = self.snapshot();

        events.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        let mut paths = events.into_iter().map(|e| e.path).peekable();

        for path in paths {
            let relative_path = path.strip_prefix(&snapshot.path)?.to_path_buf();
            let snapshot_entry = snapshot.entry_for_path(relative_path);

            if let Some(fs_entry) = self.fs_entry_for_path(&path) {
                if let Some(snapshot_entry) = snapshot_entry {
                    // If the parent does not match:
                    // Remove snapshot entry from its parent.
                    // Set its parent to none.
                    // Add its inode to a set of inodes to potentially remove after the batch.

                    // If the parent does match, continue to next path
                }

                // If we get here, we either had no snapshot entry at the path or we had the wrong
                // entry (different inode) and removed it.
                // In either case, we now need to add the entry for this path

                if let Some(existing_snapshot_entry) = snapshot.entry_for_inode(fs_entry.inode) {
                    // An entry already exists in the snapshot, but in the wrong spot.
                    // Set its parent to the correct parent
                    // Insert it in the children of its parent
                } else {
                    // An entry doesn't exist in the snapshot, this is the first time we've seen it.
                    // If this is a directory, do a recursive scan and discard subsequent events that are contained by the current path
                    // Then set the parent of the result of that scan to the correct parent
                    // Insert it in the children of that parent.
                }
            } else {
                if let Some(snapshot_entry) = snapshot_entry {
                    // Remove snapshot entry from its parent.
                    // Set its parent to none.
                    // Add its inode to a set of inodes to potentially remove after the batch.
                }
            }
        }

        // Check whether any entry whose parent was set to none is still an orphan. If so, remove it and all descedants.

        *self.snapshot.lock() = snapshot;

        Ok(())
    }

    fn fs_entry_for_path(&self, path: &Path) -> Option<Entry> {
        todo!()
    }

    // fn process_events2(&self, mut events: Vec<fsevent::Event>) -> Result<bool> {
    //     if self.notify.receiver_count() == 0 {
    //         return Ok(false);
    //     }

    //     // TODO: should we canonicalize this at the start?
    //     let root_path = self.path.canonicalize()?;
    //     let snapshot = Snapshot {
    //         id: self.id,
    //         entries: self.entries.lock().clone(),
    //         root_inode: self.root_inode(),
    //     };
    //     let mut removed = HashSet::new();
    //     let mut observed = HashSet::new();

    //     let (scan_queue_tx, scan_queue_rx) = crossbeam_channel::unbounded();

    //     events.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    //     let mut paths = events.into_iter().map(|e| e.path).peekable();
    //     while let Some(path) = paths.next() {
    //         let relative_path = path.strip_prefix(&root_path)?.to_path_buf();
    //         match fs::metadata(&path) {
    //             Ok(metadata) => {
    //                 let inode = metadata.ino();
    //                 let is_symlink = fs::symlink_metadata(&path)?.file_type().is_symlink();
    //                 let name: Arc<OsStr> = Arc::from(path.file_name().unwrap_or(OsStr::new("/")));
    //                 let mut ignore = IgnoreBuilder::new().build().add_parents(&path).unwrap();
    //                 if metadata.is_dir() {
    //                     ignore = ignore.add_child(&path).unwrap();
    //                 }
    //                 let is_ignored = ignore.matched(&path, metadata.is_dir()).is_ignore();
    //                 let parent = if path == root_path {
    //                     None
    //                 } else {
    //                     Some(fs::metadata(path.parent().unwrap())?.ino())
    //                 };

    //                 let prev_entry = snapshot.entries.get(&inode);
    //                 // If we haven't seen this inode yet, we are going to recursively scan it, so
    //                 // ignore event involving a descendant.
    //                 if prev_entry.is_none() {
    //                     while paths.peek().map_or(false, |p| p.starts_with(&path)) {
    //                         paths.next();
    //                     }
    //                 }

    //                 observed.insert(inode);
    //                 if metadata.file_type().is_dir() {
    //                     let is_ignored = is_ignored || name.as_ref() == ".git";
    //                     let dir_entry = Entry::Dir {
    //                         parent,
    //                         name,
    //                         inode,
    //                         is_symlink,
    //                         is_ignored,
    //                         children: Arc::from([]),
    //                         pending: true,
    //                     };
    //                     self.insert_entries(Some(dir_entry.clone()));

    //                     scan_queue_tx
    //                         .send(Ok(ScanJob {
    //                             ino: inode,
    //                             path: Arc::from(path),
    //                             relative_path,
    //                             dir_entry,
    //                             ignore: Some(ignore),
    //                             scan_queue: scan_queue_tx.clone(),
    //                         }))
    //                         .unwrap();
    //                 } else {
    //                     self.insert_entries(Some(Entry::File {
    //                         parent,
    //                         name,
    //                         path: PathEntry::new(inode, &relative_path, is_ignored),
    //                         inode,
    //                         is_symlink,
    //                         is_ignored,
    //                     }));
    //                 }
    //             }
    //             Err(err) => {
    //                 if err.kind() == io::ErrorKind::NotFound {
    //                     // Fill removed with the inodes of all descendants of this path.
    //                     let mut stack = Vec::new();
    //                     stack.extend(snapshot.inode_for_path(&relative_path));
    //                     while let Some(inode) = stack.pop() {
    //                         removed.insert(inode);
    //                         if let Some(Entry::Dir { children, .. }) = snapshot.entries.get(&inode)
    //                         {
    //                             stack.extend(children.iter().copied())
    //                         }
    //                     }
    //                 } else {
    //                     return Err(anyhow::Error::new(err));
    //                 }
    //             }
    //         }
    //     }
    //     drop(scan_queue_tx);

    //     let mut scanned_inodes = Vec::new();
    //     scanned_inodes.resize_with(self.thread_pool.workers(), || Ok(Vec::new()));
    //     self.thread_pool.scoped(|pool| {
    //         for worker_inodes in &mut scanned_inodes {
    //             pool.execute(|| {
    //                 let worker_inodes = worker_inodes;
    //                 while let Ok(job) = scan_queue_rx.recv() {
    //                     if let Err(err) = job.and_then(|job| {
    //                         self.scan_dir(job, Some(worker_inodes.as_mut().unwrap()))
    //                     }) {
    //                         *worker_inodes = Err(err);
    //                         break;
    //                     }
    //                 }
    //             });
    //         }
    //     });

    //     for worker_inodes in scanned_inodes {
    //         for inode in worker_inodes? {
    //             remove_counts.remove(&inode);
    //         }
    //     }
    //     self.remove_entries(remove_counts);

    //     Ok(self.notify.receiver_count() != 0)
    // }

    fn insert_entries(&self, entries: impl IntoIterator<Item = Entry>) {
        self.snapshot
            .lock()
            .entries
            .edit(entries.into_iter().map(Edit::Insert).collect::<Vec<_>>());
    }

    // fn insert_entries(&self, entries: impl IntoIterator<Item = Entry>) {
    //     let mut edits = Vec::new();
    //     let mut new_parents = HashMap::new();
    //     for entry in entries {
    //         new_parents.insert(entry.ino(), entry.parent());
    //         edits.push(Edit::Insert(entry));
    //     }

    //     let mut entries = self.snapshot.lock().entries;
    //     let prev_entries = entries.edit(edits);
    //     Self::remove_stale_children(&mut *entries, prev_entries, new_parents);
    // }

    // fn remove_entries(&self, inodes: impl IntoIterator<Item = u64>) {
    //     let mut entries = self.entries.lock();
    //     let prev_entries = entries.edit(inodes.into_iter().map(Edit::Remove).collect());
    //     Self::remove_stale_children(&mut *entries, prev_entries, HashMap::new());
    // }

    // fn remove_stale_children(
    //     tree: &mut SumTree<Entry>,
    //     prev_entries: Vec<Entry>,
    //     new_parents: HashMap<u64, Option<u64>>,
    // ) {
    //     let mut new_parent_entries = HashMap::new();

    //     for prev_entry in prev_entries {
    //         let new_parent = new_parents.get(&prev_entry.ino()).copied().flatten();
    //         if new_parent != prev_entry.parent() {
    //             if let Some(prev_parent) = prev_entry.parent() {
    //                 let (_, new_children) =
    //                     new_parent_entries.entry(prev_parent).or_insert_with(|| {
    //                         let prev_parent_entry = tree.get(&prev_parent).unwrap();
    //                         if let Entry::Dir { children, .. } = prev_parent_entry {
    //                             (prev_parent_entry.clone(), children.to_vec())
    //                         } else {
    //                             unreachable!()
    //                         }
    //                     });

    //                 if let Some(ix) = new_children.iter().position(|ino| *ino == prev_entry.ino()) {
    //                     new_children.swap_remove(ix);
    //                 }
    //             }
    //         }
    //     }

    //     let parent_edits = new_parent_entries
    //         .into_iter()
    //         .map(|(_, (mut parent_entry, new_children))| {
    //             if let Entry::Dir { children, .. } = &mut parent_entry {
    //                 *children = Arc::from(new_children);
    //             } else {
    //                 unreachable!()
    //             }
    //             Edit::Insert(parent_entry)
    //         })
    //         .collect::<Vec<_>>();
    //     tree.edit(parent_edits);
    // }
}

struct ScanJob {
    ino: u64,
    path: Arc<Path>,
    relative_path: PathBuf,
    dir_entry: Entry,
    ignore: Option<Ignore>,
    scan_queue: crossbeam_channel::Sender<io::Result<ScanJob>>,
}

pub trait WorktreeHandle {
    fn file(&self, entry_id: u64, app: &AppContext) -> Result<FileHandle>;
}

impl WorktreeHandle for ModelHandle<Worktree> {
    fn file(&self, inode: u64, app: &AppContext) -> Result<FileHandle> {
        if self.read(app).has_inode(inode) {
            Ok(FileHandle {
                worktree: self.clone(),
                inode,
            })
        } else {
            Err(anyhow!("entry does not exist in tree"))
        }
    }
}

trait UnwrapIgnoreTuple {
    fn unwrap(self) -> Ignore;
}

impl UnwrapIgnoreTuple for (Ignore, Option<ignore::Error>) {
    fn unwrap(self) -> Ignore {
        if let Some(error) = self.1 {
            log::error!("error loading gitignore data: {}", error);
        }
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Buffer;
    use crate::test::*;
    use anyhow::Result;
    use gpui::App;
    use serde_json::json;
    use std::os::unix;

    #[test]
    fn test_populate_and_search() {
        App::test_async((), |mut app| async move {
            let dir = temp_tree(json!({
                "root": {
                    "apple": "",
                    "banana": {
                        "carrot": {
                            "date": "",
                            "endive": "",
                        }
                    },
                    "fennel": {
                        "grape": "",
                    }
                }
            }));

            let root_link_path = dir.path().join("root_link");
            unix::fs::symlink(&dir.path().join("root"), &root_link_path).unwrap();

            let tree = app.add_model(|ctx| Worktree::new(root_link_path, ctx));
            assert_condition(1, 300, || app.read(|ctx| tree.read(ctx).file_count() == 4)).await;
            app.read(|ctx| {
                let tree = tree.read(ctx);
                let results = match_paths(
                    Some(tree.snapshot()).iter(),
                    "bna",
                    false,
                    false,
                    false,
                    10,
                    ctx.thread_pool().clone(),
                )
                .iter()
                .map(|result| tree.path_for_inode(result.entry_id, true))
                .collect::<Result<Vec<PathBuf>, _>>()
                .unwrap();
                assert_eq!(
                    results,
                    vec![
                        PathBuf::from("root_link/banana/carrot/date"),
                        PathBuf::from("root_link/banana/carrot/endive"),
                    ]
                );
            })
        });
    }

    #[test]
    fn test_save_file() {
        App::test_async((), |mut app| async move {
            let dir = temp_tree(json!({
                "file1": "the old contents",
            }));

            let tree = app.add_model(|ctx| Worktree::new(dir.path(), ctx));
            assert_condition(1, 300, || app.read(|ctx| tree.read(ctx).file_count() == 1)).await;

            let buffer = Buffer::new(1, "a line of text.\n".repeat(10 * 1024));

            let file_inode = app.read(|ctx| {
                let tree = tree.read(ctx);
                let inode = tree.files().next().unwrap();
                assert_eq!(
                    tree.path_for_inode(inode, false)
                        .unwrap()
                        .file_name()
                        .unwrap(),
                    "file1"
                );
                inode
            });

            tree.update(&mut app, |tree, ctx| {
                smol::block_on(tree.save(file_inode, buffer.snapshot(), ctx.as_ref())).unwrap()
            });

            let loaded_history = app
                .read(|ctx| tree.read(ctx).load_history(file_inode, ctx))
                .await
                .unwrap();
            assert_eq!(loaded_history.base_text.as_ref(), buffer.text());
        });
    }

    #[test]
    fn test_rescan() {
        App::test_async((), |mut app| async move {
            let dir2 = temp_tree(json!({
                "dir1": {
                    "dir3": {
                        "file": "contents",
                    }
                },
                "dir2": {
                }
            }));
            let dir = temp_tree(json!({
                "dir1": {
                    "dir3": {
                        "file": "contents",
                    }
                },
                "dir2": {
                }
            }));

            let tree = app.add_model(|ctx| Worktree::new(dir.path(), ctx));
            assert_condition(1, 300, || app.read(|ctx| tree.read(ctx).file_count() == 1)).await;

            let dir_inode = app.read(|ctx| {
                tree.read(ctx)
                    .snapshot()
                    .inode_for_path("dir1/dir3")
                    .unwrap()
            });
            app.read(|ctx| {
                let tree = tree.read(ctx);
                assert_eq!(
                    tree.path_for_inode(dir_inode, false)
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "dir1/dir3"
                );
            });

            std::fs::rename(dir2.path(), dir.path().join("foo")).unwrap();
            assert_condition(1, 300, || {
                app.read(|ctx| {
                    let tree = tree.read(ctx);
                    tree.path_for_inode(dir_inode, false)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        == "dir2/dir3"
                })
            })
            .await;
            app.read(|ctx| {
                let tree = tree.read(ctx);
                assert_eq!(tree.snapshot().inode_for_path("dir2/dir3"), Some(dir_inode));
            });
        });
    }
}
