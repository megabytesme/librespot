#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use priority_queue::PriorityQueue;
use thiserror::Error;

use crate::{
    Error, FileId, LibrespotKeyRemoveCallback, authentication::Credentials, error::ErrorKind,
};

const CACHE_LIMITER_POISON_MSG: &str = "cache limiter mutex should not be poisoned";
const PENDING_DELETE_FILE: &str = ".pending_delete";

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("audio cache location is not configured")]
    Path,
}

impl From<CacheError> for Error {
    fn from(err: CacheError) -> Self {
        Error::failed_precondition(err)
    }
}

/// Some kind of data structure that holds some paths, the size of these files and a timestamp.
/// It keeps track of the file sizes and is able to pop the path with the oldest timestamp if
/// a given limit is exceeded.
struct SizeLimiter {
    queue: PriorityQueue<PathBuf, Reverse<SystemTime>>,
    sizes: HashMap<PathBuf, u64>,
    size_limit: u64,
    in_use: u64,
}

impl SizeLimiter {
    /// Creates a new instance with the given size limit.
    fn new(limit: u64) -> Self {
        Self {
            queue: PriorityQueue::new(),
            sizes: HashMap::new(),
            size_limit: limit,
            in_use: 0,
        }
    }

    /// Adds an entry to this data structure.
    ///
    /// If this file is already contained, it will be updated accordingly.
    fn add(&mut self, file: &Path, size: u64, accessed: SystemTime) {
        self.in_use += size;
        self.queue.push(file.to_owned(), Reverse(accessed));
        if let Some(old_size) = self.sizes.insert(file.to_owned(), size) {
            // It's important that decreasing happens after
            // increasing the size, to prevent an overflow.
            self.in_use -= old_size;
        }
    }

    /// Returns true if the limit is exceeded.
    fn exceeds_limit(&self) -> bool {
        self.in_use > self.size_limit
    }

    /// Returns the least recently accessed file if the size of the cache exceeds
    /// the limit.
    ///
    /// The entry is removed from the data structure, but the caller is responsible
    /// to delete the file in the file system.
    fn pop(&mut self) -> Option<PathBuf> {
        if self.exceeds_limit() {
            if let Some((next, _)) = self.queue.pop() {
                if let Some(size) = self.sizes.remove(&next) {
                    self.in_use -= size;
                } else {
                    error!("`queue` and `sizes` should have the same keys.");
                }
                Some(next)
            } else {
                error!("in_use was > 0, so the queue should have contained an item.");
                None
            }
        } else {
            None
        }
    }

    /// Updates the timestamp of an existing element. Returns `true` if the item did exist.
    fn update(&mut self, file: &Path, access_time: SystemTime) -> bool {
        self.queue
            .change_priority(file, Reverse(access_time))
            .is_some()
    }

    /// Removes an element with the specified path. Returns `true` if the item did exist.
    fn remove(&mut self, file: &Path) -> bool {
        if self.queue.remove(file).is_none() {
            return false;
        }

        if let Some(size) = self.sizes.remove(file) {
            self.in_use -= size;
        } else {
            error!("`queue` and `sizes` should have the same keys.");
        }

        true
    }
}

struct FsSizeLimiter {
    limiter: Mutex<SizeLimiter>,
    volatile_root: PathBuf,
    pending_delete_file: PathBuf,
    pending_delete_lock: Mutex<()>,
}

impl FsSizeLimiter {
    /// Returns access time and file size of a given path.
    fn get_metadata(file: &Path) -> io::Result<(SystemTime, u64)> {
        let metadata = file.metadata()?;

        // The first of the following timestamps which is available will be chosen as access time:
        // 1. Access time
        // 2. Modification time
        // 3. Creation time
        // 4. Current time
        let access_time = metadata
            .accessed()
            .or_else(|_| metadata.modified())
            .or_else(|_| metadata.created())
            .unwrap_or_else(|_| SystemTime::now());

        let size = metadata.len();

        Ok((access_time, size))
    }

    /// Recursively search a directory for files and add them to the `limiter` struct.
    fn init_dir(limiter: &mut SizeLimiter, path: &Path, pending_delete_file: &Path) {
        let list_dir = match fs::read_dir(path) {
            Ok(list_dir) => list_dir,
            Err(e) => {
                warn!("Could not read directory {path:?} in cache dir: {e}");
                return;
            }
        };

        for entry in list_dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    warn!("Could not read directory {path:?} in cache dir: {e}");
                    return;
                }
            };

            let entry_path = entry.path();

            if entry_path == pending_delete_file {
                continue;
            }

            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() || file_type.is_symlink() => {
                    Self::init_dir(limiter, &entry_path, pending_delete_file)
                }
                Ok(file_type) if file_type.is_file() => match Self::get_metadata(&entry_path) {
                    Ok((access_time, size)) => limiter.add(&entry_path, size, access_time),
                    Err(e) => warn!("Could not read file {entry_path:?} in cache dir: {e}"),
                },
                Ok(ft) => warn!(
                    "File {:?} in cache dir has unsupported type {:?}",
                    entry_path, ft
                ),
                Err(e) => warn!(
                    "Could not get type of file {:?} in cache dir: {}",
                    entry_path, e
                ),
            };
        }
    }

    fn add(&self, file: &Path, size: u64) {
        self.limiter
            .lock()
            .expect(CACHE_LIMITER_POISON_MSG)
            .add(file, size, SystemTime::now())
    }

    fn touch(&self, file: &Path) -> bool {
        self.limiter
            .lock()
            .expect(CACHE_LIMITER_POISON_MSG)
            .update(file, SystemTime::now())
    }

    fn remove(&self, file: &Path) -> bool {
        self.limiter
            .lock()
            .expect(CACHE_LIMITER_POISON_MSG)
            .remove(file)
    }

    fn with_pending_delete_set<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&mut HashSet<PathBuf>) -> R,
    {
        let _guard = self
            .pending_delete_lock
            .lock()
            .expect(CACHE_LIMITER_POISON_MSG);

        let mut set = HashSet::new();

        if let Ok(content) = fs::read_to_string(&self.pending_delete_file) {
            for line in content.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    set.insert(PathBuf::from(trimmed));
                }
            }
        }

        let result = f(&mut set);

        let mut entries: Vec<_> = set.iter().collect();
        entries.sort();

        let mut contents = String::new();
        for rel in entries {
            contents.push_str(&rel.to_string_lossy());
            contents.push('\n');
        }

        if let Err(e) = fs::write(&self.pending_delete_file, contents) {
            warn!("Could not write pending delete queue: {e}");
        }

        result
    }

    fn enqueue_pending_delete(&self, file: &Path) {
        if !file.starts_with(&self.volatile_root) {
            return;
        }

        let relative = match file.strip_prefix(&self.volatile_root) {
            Ok(p) => p.to_path_buf(),
            Err(_) => return,
        };

        self.with_pending_delete_set(|set| {
            set.insert(relative);
        });
    }

    fn remove_pending_delete(&self, file: &Path) {
        if !file.starts_with(&self.volatile_root) {
            return;
        }

        let relative = match file.strip_prefix(&self.volatile_root) {
            Ok(p) => p.to_path_buf(),
            Err(_) => return,
        };

        self.with_pending_delete_set(|set| {
            set.remove(&relative);
        });
    }

    fn list_pending_deletes(&self) -> Vec<PathBuf> {
        self.with_pending_delete_set(|set| set.iter().cloned().collect())
    }

    fn invoke_remove_callback_for_path(
        &self,
        file_path: &Path,
        remove_callback: Option<LibrespotKeyRemoveCallback>,
    ) {
        let Some(cb) = remove_callback else {
            return;
        };

        let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) else {
            return;
        };

        let Some(parent) = file_path
            .parent()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()))
        else {
            return;
        };

        let full_hex_id = format!("{}{}", parent, file_name);

        if let Ok(bytes) = (0..full_hex_id.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&full_hex_id[i..i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
        {
            cb(bytes.as_ptr(), std::ptr::null_mut());
        }
    }

    fn process_pending_deletes(&self, remove_callback: Option<LibrespotKeyRemoveCallback>) {
        let entries = self.list_pending_deletes();
        if entries.is_empty() {
            return;
        }

        let mut removed = 0usize;

        for rel in entries {
            let full = self.volatile_root.join(&rel);

            if !full.exists() {
                self.remove_pending_delete(&full);
                removed += 1;
                continue;
            }

            match fs::remove_file(&full) {
                Ok(_) => {
                    self.remove_pending_delete(&full);
                    self.invoke_remove_callback_for_path(&full, remove_callback);
                    removed += 1;
                }
                Err(e) => {
                    warn!("Pending delete retry failed for {full:?}: {e}");
                }
            }
        }

        if removed > 0 {
            info!("Processed {removed} pending cache deletions.");
        }
    }

    fn prune_internal<F: FnMut() -> Option<PathBuf>>(
        &self,
        mut pop: F,
        remove_callback: Option<LibrespotKeyRemoveCallback>,
    ) -> Result<(), Error> {
        let mut first = true;
        let mut count = 0;
        let mut last_error = None;

        self.process_pending_deletes(remove_callback);

        while let Some(file_path) = pop() {
            if first {
                debug!("Cache dir exceeds limit, removing least recently used files.");
                first = false;
            }

            match fs::remove_file(&file_path) {
                Ok(_) => {
                    self.remove_pending_delete(&file_path);
                    self.invoke_remove_callback_for_path(&file_path, remove_callback);
                    count += 1;
                }
                Err(e) => {
                    warn!(
                        "Could not remove file {file_path:?} from cache dir: {e}; queued for retry"
                    );
                    self.enqueue_pending_delete(&file_path);
                    last_error = Some(e);
                }
            }
        }

        self.process_pending_deletes(remove_callback);

        if count > 0 {
            info!("Removed {count} cache files.");
        }

        if let Some(err) = last_error {
            Err(err.into())
        } else {
            Ok(())
        }
    }

    fn prune(&self, remove_callback: Option<LibrespotKeyRemoveCallback>) -> Result<(), Error> {
        self.prune_internal(
            || self.limiter.lock().expect(CACHE_LIMITER_POISON_MSG).pop(),
            remove_callback,
        )
    }

    fn new(
        path: &Path,
        limit: u64,
        remove_callback: Option<LibrespotKeyRemoveCallback>,
    ) -> Result<Self, Error> {
        let pending_delete_file = path.join(PENDING_DELETE_FILE);
        let mut limiter = SizeLimiter::new(limit);

        Self::init_dir(&mut limiter, path, &pending_delete_file);

        let this = Self {
            limiter: Mutex::new(limiter),
            volatile_root: path.to_path_buf(),
            pending_delete_file,
            pending_delete_lock: Mutex::new(()),
        };

        this.process_pending_deletes(remove_callback);
        this.prune(remove_callback)?;

        Ok(this)
    }
}

#[derive(Clone)]
pub struct Cache {
    credentials_location: Option<PathBuf>,
    volume_location: Option<PathBuf>,
    audio_location: Option<PathBuf>,
    persisted_audio_location: Option<PathBuf>,
    size_limiter: Option<Arc<FsSizeLimiter>>,
    remove_callback: Option<LibrespotKeyRemoveCallback>,
}

impl Cache {
    pub fn new<P: AsRef<Path>>(
        credentials_path: Option<P>,
        volume_path: Option<P>,
        audio_path: Option<P>,
        persisted_audio_path: Option<P>,
        size_limit: Option<u64>,
        remove_callback: Option<LibrespotKeyRemoveCallback>,
    ) -> Result<Self, Error> {
        let mut size_limiter = None;

        if let Some(location) = &credentials_path {
            fs::create_dir_all(location)?;
        }
        let credentials_location = credentials_path
            .as_ref()
            .map(|p| p.as_ref().join("credentials.json"));

        if let Some(location) = &volume_path {
            fs::create_dir_all(location)?;
        }
        let volume_location = volume_path.as_ref().map(|p| p.as_ref().join("volume"));

        if let Some(location) = &audio_path {
            fs::create_dir_all(location)?;
            if let Some(limit) = size_limit {
                let limiter = FsSizeLimiter::new(location.as_ref(), limit, remove_callback)?;
                size_limiter = Some(Arc::new(limiter));
            }
        }

        if let Some(location) = &persisted_audio_path {
            fs::create_dir_all(location)?;
        }

        let audio_location = audio_path.map(|p| p.as_ref().to_owned());
        let persisted_audio_location = persisted_audio_path.map(|p| p.as_ref().to_owned());

        Ok(Self {
            credentials_location,
            volume_location,
            audio_location,
            persisted_audio_location,
            size_limiter,
            remove_callback,
        })
    }

    fn file_subpath(file: FileId) -> PathBuf {
        let name = file.to_base16();
        let mut path = PathBuf::from(&name[0..2]);
        path.push(&name[2..]);
        path
    }

    pub fn volatile_file_path(&self, file: FileId) -> Option<PathBuf> {
        self.audio_location
            .as_ref()
            .map(|root| root.join(Self::file_subpath(file)))
    }

    pub fn persisted_file_path(&self, file: FileId) -> Option<PathBuf> {
        self.persisted_audio_location
            .as_ref()
            .map(|root| root.join(Self::file_subpath(file)))
    }

    pub fn file_path(&self, file: FileId) -> Option<PathBuf> {
        if let Some(path) = self.persisted_file_path(file) {
            if path.exists() {
                return Some(path);
            }
        }

        if let Some(path) = self.volatile_file_path(file) {
            if path.exists() {
                return Some(path);
            }
        }

        self.persisted_file_path(file)
            .or_else(|| self.volatile_file_path(file))
    }

    pub fn set_persisted(&self, file: FileId, persisted: bool) -> Result<(), Error> {
        let volatile = self.volatile_file_path(file).ok_or(CacheError::Path)?;
        let stable = self.persisted_file_path(file).ok_or(CacheError::Path)?;

        let (src, dst) = if persisted {
            (&volatile, &stable)
        } else {
            (&stable, &volatile)
        };

        if !src.exists() {
            return Err(Error::not_found(format!("cache file not found: {src:?}")));
        }

        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::rename(src, dst)?;

        if let Some(limiter) = self.size_limiter.as_deref() {
            if persisted {
                limiter.remove(&volatile);
                limiter.remove_pending_delete(&volatile);
            } else {
                let (_, size) = FsSizeLimiter::get_metadata(&volatile)?;
                limiter.add(&volatile, size);
                limiter.prune(self.remove_callback)?;
            }
        }

        Ok(())
    }

    pub fn credentials(&self) -> Option<Credentials> {
        let location = self.credentials_location.as_ref()?;

        // This closure is just convencience to enable the question mark operator
        let read = || -> Result<Credentials, Error> {
            let file = File::open(location)?;
            #[cfg(unix)]
            if file.metadata()?.mode() & 0o004 != 0 {
                warn!(
                    "credential file {location:?} is currently world readable, consider using chmod 600 {location:?} to fix this"
                )
            }
            Ok(serde_json::from_reader(file)?)
        };

        match read() {
            Ok(c) => Some(c),
            Err(e) => {
                // If the file did not exist, the file was probably not written
                // before. Otherwise, log the error.
                if e.kind != ErrorKind::NotFound {
                    warn!("Error reading credentials from cache: {e}");
                }
                None
            }
        }
    }

    pub fn save_credentials(&self, cred: &Credentials) {
        if let Some(location) = &self.credentials_location {
            let mut file = File::options();
            #[cfg(unix)]
            let file = file.mode(0o600);
            let result = file
                .create(true)
                .write(true)
                .truncate(true)
                .open(location)
                .and_then(|file| Ok(serde_json::to_writer(file, cred)?));

            if let Err(e) = result {
                warn!("Cannot save credentials to cache: {e}")
            }
        }
    }

    pub fn volume(&self) -> Option<u16> {
        let location = self.volume_location.as_ref()?;

        let read = || -> Result<u16, Error> {
            let mut file = File::open(location)?;
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            Ok(contents.parse()?)
        };

        match read() {
            Ok(v) => Some(v),
            Err(e) => {
                if e.kind != ErrorKind::NotFound {
                    warn!("Error reading volume from cache: {e}");
                }
                None
            }
        }
    }

    pub fn save_volume(&self, volume: u16) {
        if let Some(ref location) = self.volume_location {
            let result = File::create(location).and_then(|mut file| write!(file, "{volume}"));
            if let Err(e) = result {
                warn!("Cannot save volume to cache: {e}");
            }
        }
    }

    pub fn file(&self, file: FileId) -> Option<File> {
        let path = self.file_path(file)?;
        match File::open(&path) {
            Ok(file_handle) => {
                if let Some(limiter) = self.size_limiter.as_deref() {
                    if self
                        .audio_location
                        .as_ref()
                        .is_some_and(|root| path.starts_with(root))
                    {
                        if !limiter.touch(&path) {
                            error!("limiter could not touch {path:?}");
                        }
                    }
                }
                Some(file_handle)
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    warn!("Error reading file from cache: {e}");
                }
                None
            }
        }
    }

    pub fn save_file<F: Read>(&self, file: FileId, contents: &mut F) -> Result<PathBuf, Error> {
        let path = self.volatile_file_path(file).ok_or(CacheError::Path)?;

        if let Some(parent) = path.parent() {
            let size = fs::create_dir_all(parent)
                .and_then(|_| File::create(&path))
                .and_then(|mut file| io::copy(contents, &mut file))?;

            if let Some(limiter) = self.size_limiter.as_deref() {
                limiter.remove_pending_delete(&path);
                limiter.add(&path, size);
                limiter.prune(self.remove_callback)?;
            }

            Ok(path)
        } else {
            Err(CacheError::Path.into())
        }
    }

    pub fn remove_file(&self, file: FileId) -> Result<(), Error> {
        let volatile = self.volatile_file_path(file);
        let stable = self.persisted_file_path(file);

        let path = match (stable.as_ref(), volatile.as_ref()) {
            (Some(p), _) if p.exists() => p.clone(),
            (_, Some(p)) if p.exists() => p.clone(),
            _ => return Err(CacheError::Path.into()),
        };

        match fs::remove_file(&path) {
            Ok(_) => {
                if let Some(cb) = self.remove_callback {
                    let hex_id = file.to_base16();
                    if let Ok(bytes) = (0..hex_id.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&hex_id[i..i + 2], 16))
                        .collect::<Result<Vec<u8>, _>>()
                    {
                        cb(bytes.as_ptr(), std::ptr::null_mut());
                    }
                }
            }
            Err(e) => {
                if self
                    .audio_location
                    .as_ref()
                    .is_some_and(|root| path.starts_with(root))
                {
                    if let Some(limiter) = self.size_limiter.as_deref() {
                        limiter.remove(&path);
                        limiter.enqueue_pending_delete(&path);
                    }
                    return Ok(());
                } else {
                    return Err(e.into());
                }
            }
        }

        if let Some(limiter) = self.size_limiter.as_deref() {
            if self
                .audio_location
                .as_ref()
                .is_some_and(|root| path.starts_with(root))
            {
                limiter.remove(&path);
                limiter.remove_pending_delete(&path);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;

    fn ordered_time(v: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(v)
    }

    #[test]
    fn test_size_limiter() {
        let mut limiter = SizeLimiter::new(1000);

        limiter.add(Path::new("a"), 500, ordered_time(2));
        limiter.add(Path::new("b"), 500, ordered_time(1));

        // b (500) -> a (500)  => sum: 1000 <= 1000
        assert!(!limiter.exceeds_limit());
        assert_eq!(limiter.pop(), None);

        limiter.add(Path::new("c"), 1000, ordered_time(3));

        // b (500) -> a (500) -> c (1000)  => sum: 2000 > 1000
        assert!(limiter.exceeds_limit());
        assert_eq!(limiter.pop().as_deref(), Some(Path::new("b")));
        // a (500) -> c (1000)  => sum: 1500 > 1000
        assert_eq!(limiter.pop().as_deref(), Some(Path::new("a")));
        // c (1000)   => sum: 1000 <= 1000
        assert_eq!(limiter.pop().as_deref(), None);

        limiter.add(Path::new("d"), 5, ordered_time(2));
        // d (5) -> c (1000) => sum: 1005 > 1000
        assert_eq!(limiter.pop().as_deref(), Some(Path::new("d")));
        // c (1000)   => sum: 1000 <= 1000
        assert_eq!(limiter.pop().as_deref(), None);

        // Test updating

        limiter.add(Path::new("e"), 500, ordered_time(3));
        //  c (1000) -> e (500)  => sum: 1500 > 1000
        assert!(limiter.update(Path::new("c"), ordered_time(4)));
        // e (500) -> c (1000)  => sum: 1500 > 1000
        assert_eq!(limiter.pop().as_deref(), Some(Path::new("e")));
        // c (1000)  => sum: 1000 <= 1000

        // Test removing
        limiter.add(Path::new("f"), 500, ordered_time(2));
        assert!(limiter.remove(Path::new("c")));
        assert!(!limiter.exceeds_limit());
    }
}
