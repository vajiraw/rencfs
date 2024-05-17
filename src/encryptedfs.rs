use std::backtrace::Backtrace;
use std::cmp::max;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::fs::{DirEntry, File, OpenOptions, ReadDir};
use std::io::ErrorKind::Other;
use std::io::{SeekFrom, Write};
use std::num::{NonZeroUsize, ParseIntError};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, LazyLock, Weak};
use std::time::{Duration, SystemTime};
use std::{fs, io};

use argon2::password_hash::rand_core::RngCore;
use futures_util::TryStreamExt;
use lru::LruCache;
use num_format::{Locale, ToFormattedString};
use secrecy::{ExposeSecret, SecretString, SecretVec};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::runtime::Runtime;
use tokio::sync::{Mutex, RwLock};
use tokio::task::{JoinError, JoinSet};
use tokio_stream::wrappers::ReadDirStream;
use tracing::{debug, error, info, instrument, warn};

use crate::arc_hashmap::{ArcHashMap, Holder};
use crate::async_util::call_async;
use crate::crypto::reader::CryptoReader;
use crate::crypto::writer::{
    CryptoWriter, CryptoWriterSeek, FileCryptoWriterCallback, FileCryptoWriterMetadataProvider,
};
use crate::crypto::Cipher;
use crate::expire_value::{ExpireValue, ValueProvider};
use crate::{crypto, fs_util, stream_util};

#[cfg(test)]
pub(crate) mod test;

pub(crate) const INODES_DIR: &str = "inodes";
pub(crate) const CONTENTS_DIR: &str = "contents";
pub(crate) const SECURITY_DIR: &str = "security";
pub(crate) const KEY_ENC_FILENAME: &str = "key.enc";
pub(crate) const KEY_SALT_FILENAME: &str = "key.salt";

const LS_DIR: &str = "ls";
const HASH_DIR: &str = "hash";

pub(crate) const ROOT_INODE: u64 = 1;

fn spawn_runtime() -> Runtime {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
}

static DIR_ENTRIES_RT: LazyLock<Runtime> = LazyLock::new(|| spawn_runtime());
static NOD_RT: LazyLock<Runtime> = LazyLock::new(|| spawn_runtime());

async fn reset_handles(
    fs: Arc<EncryptedFs>,
    ino: u64,
    changed_from_pos: i64,
    last_write_pos: u64,
    fh: u64,
) -> FsResult<()> {
    {
        let mut attr = fs.get_inode_from_storage(ino, fs.key.get().await?)?;
        let lock = fs
            .serialize_update_inode_locks
            .get_or_insert_with(ino, || Mutex::new(false));
        let _guard = lock.lock().await;
        // if we wrote pass the filesize we need to update the filesize
        if last_write_pos > attr.size {
            attr.size = last_write_pos;
            fs.write_inode_to_storage(&attr).await?;
        }
    }
    fs.reset_handles(ino, changed_from_pos, Some(fh)).await?;
    Ok(())
}

async fn get_metadata(fs: Arc<EncryptedFs>, ino: u64) -> FsResult<FileAttr> {
    let lock = fs.attr_cache.get().await?;
    let mut guard = lock.lock().await;
    let attr = guard.get(&ino);
    if let Some(attr) = attr {
        Ok(*attr)
    } else {
        let attr = fs.get_inode_from_storage(ino, fs.key.get().await?)?;
        guard.put(ino, attr);
        Ok(attr)
    }
}

struct LocalFileCryptoWriterMetadataProvider(Weak<EncryptedFs>, u64);
impl FileCryptoWriterMetadataProvider for LocalFileCryptoWriterMetadataProvider {
    fn size(&self) -> io::Result<u64> {
        debug!("requesting size info");
        call_async(async {
            if let Some(fs) = self.0.upgrade() {
                get_metadata(fs, self.1)
                    .await
                    .map_err(|e| io::Error::new(Other, e))
                    .map(|attr| attr.size)
            } else {
                Err(io::Error::new(Other, "fs dropped"))
            }
        })
    }
}

/// File attributes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileAttr {
    /// Inode number
    pub ino: u64,
    /// Size in bytes
    pub size: u64,
    /// Size in blocks
    pub blocks: u64,
    /// Time of last access
    pub atime: SystemTime,
    /// Time of last modification
    pub mtime: SystemTime,
    /// Time of last change
    pub ctime: SystemTime,
    /// Time of creation (macOS only)
    pub crtime: SystemTime,
    /// Kind of file (directory, file, pipe, etc.)
    pub kind: FileType,
    /// Permissions
    pub perm: u16,
    /// Number of hard links
    pub nlink: u32,
    /// User id
    pub uid: u32,
    /// Group id
    pub gid: u32,
    /// Rdev
    pub rdev: u32,
    /// Block size
    pub blksize: u32,
    /// Flags (macOS only, see chflags(2))
    pub flags: u32,
}

/// File types.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum FileType {
    // /// Named pipe (S_IFIFO)
    // NamedPipe,
    // /// Character device (S_IFCHR)
    // CharDevice,
    // /// Block device (S_IFBLK)
    // BlockDevice,
    /// Directory (`S_IFDIR`)
    Directory,
    /// Regular file (`S_IFREG`)
    RegularFile,
    // /// Symbolic link (S_IFLNK)
    // Symlink,
    // /// Unix domain socket (S_IFSOCK)
    // Socket,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SetFileAttr {
    /// Size in bytes
    pub size: Option<u64>,
    /// Time of last access
    pub atime: Option<SystemTime>,
    /// Time of last modification
    pub mtime: Option<SystemTime>,
    /// Time of last change
    pub ctime: Option<SystemTime>,
    /// Time of creation (macOS only)
    pub crtime: Option<SystemTime>,
    /// Permissions
    pub perm: Option<u16>,
    /// User id
    pub uid: Option<u32>,
    /// Group id
    pub gid: Option<u32>,
    /// Rdev
    pub rdev: Option<u32>,
    /// Flags (macOS only, see chflags(2))
    pub flags: Option<u32>,
}

impl SetFileAttr {
    #[must_use]
    pub const fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    #[must_use]
    pub const fn with_atime(mut self, atime: SystemTime) -> Self {
        self.atime = Some(atime);
        self
    }

    #[must_use]
    pub const fn with_mtime(mut self, mtime: SystemTime) -> Self {
        self.mtime = Some(mtime);
        self
    }

    #[must_use]
    pub const fn with_ctime(mut self, ctime: SystemTime) -> Self {
        self.ctime = Some(ctime);
        self
    }

    #[must_use]
    pub const fn with_crtime(mut self, crtime: SystemTime) -> Self {
        self.crtime = Some(crtime);
        self
    }

    #[must_use]
    pub const fn with_perm(mut self, perm: u16) -> Self {
        self.perm = Some(perm);
        self
    }

    #[must_use]
    pub const fn with_uid(mut self, uid: u32) -> Self {
        self.uid = Some(uid);
        self
    }

    #[must_use]
    pub const fn with_gid(mut self, gid: u32) -> Self {
        self.gid = Some(gid);
        self
    }

    #[must_use]
    pub const fn with_rdev(mut self, rdev: u32) -> Self {
        self.rdev = Some(rdev);
        self
    }

    #[must_use]
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.rdev = Some(flags);
        self
    }
}

#[derive(Debug, Clone)]
pub struct CreateFileAttr {
    /// Kind of file (directory, file, pipe, etc.)
    pub kind: FileType,
    /// Permissions
    pub perm: u16,
    /// User id
    pub uid: u32,
    /// Group id
    pub gid: u32,
    /// Rdev
    pub rdev: u32,
    /// Flags (macOS only, see chflags(2))
    pub flags: u32,
}

impl From<CreateFileAttr> for FileAttr {
    fn from(value: CreateFileAttr) -> Self {
        Self {
            ino: 0,
            size: 0,
            blocks: 0,
            atime: SystemTime::now(),
            mtime: SystemTime::now(),
            ctime: SystemTime::now(),
            crtime: SystemTime::now(),
            kind: value.kind,
            perm: value.perm,
            nlink: if value.kind == FileType::Directory {
                2
            } else {
                1
            },
            uid: value.uid,
            gid: value.gid,
            rdev: value.rdev,
            blksize: 0,
            flags: value.flags,
        }
    }
}

#[derive(Error, Debug)]
pub enum FsError {
    #[error("IO error: {source}")]
    Io {
        #[from]
        source: io::Error,
        backtrace: Backtrace,
    },
    #[error("serialize error: {source}")]
    SerializeError {
        #[from]
        source: bincode::Error,
        backtrace: Backtrace,
    },
    #[error("item not found")]
    NotFound(&'static str),
    #[error("inode not found")]
    InodeNotFound,
    #[error("invalid input")]
    InvalidInput(&'static str),
    #[error("invalid node type")]
    InvalidInodeType,
    #[error("invalid file handle")]
    InvalidFileHandle,
    #[error("already exists")]
    AlreadyExists,
    #[error("already open for write")]
    AlreadyOpenForWrite,
    #[error("not empty")]
    NotEmpty,
    #[error("other")]
    Other(&'static str),
    #[error("invalid password")]
    InvalidPassword,
    #[error("invalid structure of data directory")]
    InvalidDataDirStructure,
    #[error("crypto error: {source}")]
    Crypto {
        #[from]
        source: crypto::Error,
        backtrace: Backtrace,
    },
    #[error("keyring error: {source}")]
    Keyring {
        #[from]
        source: keyring::Error,
        backtrace: Backtrace,
    },
    #[error("parse int error: {source}")]
    ParseIntError {
        #[from]
        source: ParseIntError,
        backtrace: Backtrace,
    },
    #[error("tokio join error: {source}")]
    JoinError {
        #[from]
        source: JoinError,
        backtrace: Backtrace,
    },
    #[error("max filesize exceeded, max allowed {0}")]
    MaxFilesizeExceeded(usize),
}

#[derive(Debug, Clone)]
struct TimeAndSizeFileAttr {
    atime: SystemTime,
    mtime: SystemTime,
    ctime: SystemTime,
    crtime: SystemTime,
    size: u64,
}

impl TimeAndSizeFileAttr {
    #[allow(dead_code)]
    const fn new(
        atime: SystemTime,
        mtime: SystemTime,
        ctime: SystemTime,
        crtime: SystemTime,
        size: u64,
    ) -> Self {
        Self {
            atime,
            mtime,
            ctime,
            crtime,
            size,
        }
    }
}

impl From<FileAttr> for TimeAndSizeFileAttr {
    fn from(value: FileAttr) -> Self {
        Self {
            atime: value.atime,
            mtime: value.mtime,
            ctime: value.ctime,
            crtime: value.crtime,
            size: value.size,
        }
    }
}

impl From<TimeAndSizeFileAttr> for SetFileAttr {
    fn from(value: TimeAndSizeFileAttr) -> Self {
        Self::default()
            .with_atime(value.atime)
            .with_mtime(value.mtime)
            .with_ctime(value.ctime)
            .with_crtime(value.crtime)
            .with_size(value.size)
    }
}

#[derive(Debug, Clone)]
pub struct DirectoryEntry {
    pub ino: u64,
    pub name: SecretString,
    pub kind: FileType,
}

impl PartialEq for DirectoryEntry {
    fn eq(&self, other: &Self) -> bool {
        self.ino == other.ino
            && self.name.expose_secret() == other.name.expose_secret()
            && self.kind == other.kind
    }
}

/// Like [`DirectoryEntry`] but with [`FileAttr`].
#[derive(Debug)]
pub struct DirectoryEntryPlus {
    pub ino: u64,
    pub name: SecretString,
    pub kind: FileType,
    pub attr: FileAttr,
}

impl PartialEq for DirectoryEntryPlus {
    fn eq(&self, other: &Self) -> bool {
        self.ino == other.ino
            && self.name.expose_secret() == other.name.expose_secret()
            && self.kind == other.kind
            && self.attr == other.attr
    }
}

pub type FsResult<T> = Result<T, FsError>;

pub struct DirectoryEntryIterator(VecDeque<FsResult<DirectoryEntry>>);

impl Iterator for DirectoryEntryIterator {
    type Item = FsResult<DirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop_front()
    }
}

pub struct DirectoryEntryPlusIterator(VecDeque<FsResult<DirectoryEntryPlus>>);

impl Iterator for DirectoryEntryPlusIterator {
    type Item = FsResult<DirectoryEntryPlus>;

    #[instrument(name = "DirectoryEntryPlusIterator::next", skip(self))]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop_front()
    }
}

struct ReadHandleContext {
    ino: u64,
    attr: TimeAndSizeFileAttr,
    reader: Option<Box<dyn CryptoReader>>,
}

enum ReadHandleContextOperation {
    Create { ino: u64 },
}

impl ReadHandleContextOperation {
    const fn get_ino(&self) -> u64 {
        match *self {
            Self::Create { ino, .. } => ino,
        }
    }
}

enum WriteHandleContextOperation {
    Create { ino: u64 },
}

impl WriteHandleContextOperation {
    const fn get_ino(&self) -> u64 {
        match *self {
            Self::Create { ino, .. } => ino,
        }
    }
}

struct WriteHandleContext {
    ino: u64,
    attr: TimeAndSizeFileAttr,
    writer: Option<Box<dyn CryptoWriterSeek<File>>>,
}

struct KeyProvider {
    key_path: PathBuf,
    salt_path: PathBuf,
    password_provider: Box<dyn PasswordProvider>,
    cipher: Cipher,
}

impl ValueProvider<SecretVec<u8>, FsError> for KeyProvider {
    fn provide(&self) -> Result<SecretVec<u8>, FsError> {
        let password = self
            .password_provider
            .get_password()
            .ok_or(FsError::InvalidPassword)?;
        read_or_create_key(&self.key_path, &self.salt_path, &password, self.cipher)
    }
}

pub trait PasswordProvider: Send + Sync + 'static {
    fn get_password(&self) -> Option<SecretString>;
}

struct DirEntryNameCacheProvider {}
impl ValueProvider<Mutex<LruCache<String, SecretString>>, FsError> for DirEntryNameCacheProvider {
    fn provide(&self) -> Result<Mutex<LruCache<String, SecretString>>, FsError> {
        Ok(Mutex::new(LruCache::new(NonZeroUsize::new(2000).unwrap())))
    }
}

struct DirEntryMetaCacheProvider {}
impl ValueProvider<Mutex<DirEntryMetaCache>, FsError> for DirEntryMetaCacheProvider {
    fn provide(&self) -> Result<Mutex<DirEntryMetaCache>, FsError> {
        Ok(Mutex::new(LruCache::new(NonZeroUsize::new(2000).unwrap())))
    }
}

struct AttrCacheProvider {}
impl ValueProvider<Mutex<LruCache<u64, FileAttr>>, FsError> for AttrCacheProvider {
    fn provide(&self) -> Result<Mutex<LruCache<u64, FileAttr>>, FsError> {
        Ok(Mutex::new(LruCache::new(NonZeroUsize::new(2000).unwrap())))
    }
}

type DirEntryMetaCache = LruCache<String, (u64, FileType)>;

/// Encrypted FS that stores encrypted files in a dedicated directory with a specific structure based on `inode`.
pub struct EncryptedFs {
    pub(crate) data_dir: PathBuf,
    write_handles: RwLock<HashMap<u64, Mutex<WriteHandleContext>>>,
    read_handles: RwLock<HashMap<u64, Mutex<ReadHandleContext>>>,
    current_handle: AtomicU64,
    cipher: Cipher,
    // (ino, fh)
    opened_files_for_read: RwLock<HashMap<u64, HashSet<u64>>>,
    opened_files_for_write: RwLock<HashMap<u64, u64>>,
    // used for rw ops of actual serialization
    // use std::sync::RwLock instead of tokio::sync::RwLock because we need to use it also in sync code in `DirectoryEntryIterator` and `DirectoryEntryPlusIterator`
    serialize_inode_locks: Arc<ArcHashMap<u64, RwLock<bool>>>,
    // used for the update op
    serialize_update_inode_locks: ArcHashMap<u64, Mutex<bool>>,
    // use std::sync::RwLock instead of tokio::sync::RwLock because we need to use it also in sync code in `DirectoryEntryIterator` and `DirectoryEntryPlusIterator`
    serialize_dir_entries_ls_locks: Arc<ArcHashMap<String, RwLock<bool>>>,
    serialize_dir_entries_hash_locks: Arc<ArcHashMap<String, RwLock<bool>>>,
    read_write_locks: ArcHashMap<u64, RwLock<bool>>,
    key: ExpireValue<SecretVec<u8>, FsError, KeyProvider>,
    self_weak: std::sync::Mutex<Option<Weak<Self>>>,
    attr_cache: ExpireValue<Mutex<LruCache<u64, FileAttr>>, FsError, AttrCacheProvider>,
    dir_entries_name_cache:
        ExpireValue<Mutex<LruCache<String, SecretString>>, FsError, DirEntryNameCacheProvider>,
    dir_entries_meta_cache:
        ExpireValue<Mutex<DirEntryMetaCache>, FsError, DirEntryMetaCacheProvider>,
}

impl EncryptedFs {
    #[allow(clippy::missing_panics_doc)]
    #[allow(clippy::missing_errors_doc)]
    pub async fn new(
        data_dir: PathBuf,
        password_provider: Box<dyn PasswordProvider>,
        cipher: Cipher,
    ) -> FsResult<Arc<Self>> {
        let key_provider = KeyProvider {
            key_path: data_dir.join(SECURITY_DIR).join(KEY_ENC_FILENAME),
            salt_path: data_dir.join(SECURITY_DIR).join(KEY_SALT_FILENAME),
            password_provider,
            cipher,
        };
        let key = ExpireValue::new(key_provider, Duration::from_secs(10 * 60));

        ensure_structure_created(&data_dir.clone()).await?;
        key.get().await?; // this will check the password

        let fs = Self {
            data_dir,
            write_handles: RwLock::new(HashMap::new()),
            read_handles: RwLock::new(HashMap::new()),
            current_handle: AtomicU64::new(1),
            cipher,
            opened_files_for_read: RwLock::new(HashMap::new()),
            opened_files_for_write: RwLock::new(HashMap::new()),
            serialize_inode_locks: Arc::new(ArcHashMap::default()),
            serialize_update_inode_locks: ArcHashMap::default(),
            serialize_dir_entries_ls_locks: Arc::new(ArcHashMap::default()),
            serialize_dir_entries_hash_locks: Arc::new(ArcHashMap::default()),
            // todo: take duration from param
            key,
            self_weak: std::sync::Mutex::new(None),
            read_write_locks: ArcHashMap::default(),
            // todo: take duration from param
            attr_cache: ExpireValue::new(AttrCacheProvider {}, Duration::from_secs(10 * 60)),
            dir_entries_name_cache: ExpireValue::new(
                DirEntryNameCacheProvider {},
                Duration::from_secs(10 * 60),
            ),
            dir_entries_meta_cache: ExpireValue::new(
                DirEntryMetaCacheProvider {},
                Duration::from_secs(10 * 60),
            ),
        };

        let arc = Arc::new(fs);
        arc.self_weak
            .lock()
            .expect("cannot obtain lock")
            .replace(Arc::downgrade(&arc));

        arc.ensure_root_exists().await?;

        Ok(arc)
    }

    pub fn node_exists(&self, ino: u64) -> bool {
        self.ino_file(ino).is_file()
    }

    pub fn is_dir(&self, ino: u64) -> FsResult<bool> {
        Ok(self.contents_path(ino).is_dir())
    }

    pub fn is_file(&self, ino: u64) -> FsResult<bool> {
        Ok(self.contents_path(ino).is_file())
    }

    /// Create a new node in the filesystem
    #[allow(clippy::missing_panics_doc)]
    #[allow(clippy::missing_errors_doc)]
    #[allow(clippy::too_many_lines)]
    pub async fn create_nod(
        &self,
        parent: u64,
        name: &SecretString,
        create_attr: CreateFileAttr,
        read: bool,
        write: bool,
    ) -> FsResult<(u64, FileAttr)> {
        if name.expose_secret() == "." || name.expose_secret() == ".." {
            return Err(FsError::InvalidInput("name cannot be '.' or '..'"));
        }
        if !self.node_exists(parent) {
            return Err(FsError::InodeNotFound);
        }
        if self.exists_by_name(parent, name)? {
            return Err(FsError::AlreadyExists);
        }

        // spawn on a dedicated runtime to not interfere with other more priority tasks
        let self_clone = self
            .self_weak
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        let name_clone = name.clone();
        NOD_RT
            .spawn(async move {
                let mut attr: FileAttr = create_attr.into();
                attr.ino = self_clone.generate_next_inode();

                let fs = self_clone;
                let mut join_set = JoinSet::new();

                // write inode
                let self_clone = fs.clone();
                let attr_clone = attr;
                self_clone.write_inode_to_storage(&attr_clone).await?;

                match attr.kind {
                    FileType::RegularFile => {
                        let self_clone = fs.clone();
                        join_set.spawn(async move {
                            // create in contents directory
                            let file = File::create(self_clone.contents_path(attr.ino))?;
                            // sync_all file and parent
                            // these operations are a bit slow, but are needed to make sure the file is correctly created
                            // i.e. creating 100 files takes 0.965 sec with sync_all and 0.130 sec without
                            file.sync_all()?;
                            File::open(
                                self_clone
                                    .contents_path(attr.ino)
                                    .parent()
                                    .expect("oops, we don't have a parent"),
                            )?
                            .sync_all()?;
                            Ok::<(), FsError>(())
                        });
                    }
                    FileType::Directory => {
                        let self_clone = fs.clone();
                        join_set.spawn(async move {
                            // create in contents directory
                            let contents_dir = self_clone.contents_path(attr.ino);
                            fs::create_dir(contents_dir.clone())?;
                            // used to keep encrypted file names used by [`read_dir`] and [`read_dir_plus`]
                            fs::create_dir(contents_dir.join(LS_DIR))?;
                            // used to keep hashes of encrypted file names used by [`exists_by_name`] and [`find_by_name`]
                            // this optimizes the search process as we don't need to decrypt all file names and search
                            fs::create_dir(contents_dir.join(HASH_DIR))?;

                            // add "." and ".." entries
                            self_clone
                                .insert_directory_entry(
                                    attr_clone.ino,
                                    &DirectoryEntry {
                                        ino: attr_clone.ino,
                                        name: SecretString::from_str("$.").expect("cannot parse"),
                                        kind: FileType::Directory,
                                    },
                                )
                                .await?;
                            self_clone
                                .insert_directory_entry(
                                    attr_clone.ino,
                                    &DirectoryEntry {
                                        ino: parent,
                                        name: SecretString::from_str("$..").expect("cannot parse"),
                                        kind: FileType::Directory,
                                    },
                                )
                                .await?;
                            Ok::<(), FsError>(())
                        });
                    }
                }

                // edd entry in parent directory, used for listing
                let self_clone = fs.clone();
                let attr_clone = attr;
                join_set.spawn(async move {
                    self_clone
                        .insert_directory_entry(
                            parent,
                            &DirectoryEntry {
                                ino: attr_clone.ino,
                                name: name_clone,
                                kind: attr_clone.kind,
                            },
                        )
                        .await?;
                    Ok::<(), FsError>(())
                });

                let self_clone = fs.clone();
                join_set.spawn(async move {
                    self_clone
                        .update_inode(
                            parent,
                            SetFileAttr::default()
                                .with_mtime(SystemTime::now())
                                .with_ctime(SystemTime::now()),
                        )
                        .await?;
                    Ok::<(), FsError>(())
                });

                let self_clone = fs.clone();
                let handle = if attr.kind == FileType::RegularFile {
                    if read || write {
                        self_clone.open(attr.ino, read, write).await?
                    } else {
                        // we don't create handle for files that are not opened
                        0
                    }
                } else {
                    // we don't use handle for directories
                    0
                };

                // wait for all tasks to finish
                while let Some(res) = join_set.join_next().await {
                    res??;
                }
                Ok((handle, attr))
            })
            .await?
    }

    #[allow(clippy::missing_panics_doc)]
    #[allow(clippy::missing_errors_doc)]
    pub async fn find_by_name(
        &self,
        parent: u64,
        name: &SecretString,
    ) -> FsResult<Option<FileAttr>> {
        if !self.node_exists(parent) {
            return Err(FsError::InodeNotFound);
        }
        if !self.is_dir(parent)? {
            return Err(FsError::InvalidInodeType);
        }
        let hash = hex::encode(crypto::hash_secret_string(name));
        let hash_path = self.contents_path(parent).join(HASH_DIR).join(hash);
        if !hash_path.is_file() {
            return Ok(None);
        }
        let lock = self
            .serialize_dir_entries_hash_locks
            .get_or_insert_with(hash_path.to_str().unwrap().to_string(), || {
                RwLock::new(false)
            });
        let guard = lock.read().await;
        let (ino, _, _): (u64, FileType, String) = bincode::deserialize_from(
            crypto::create_reader(File::open(hash_path)?, self.cipher, self.key.get().await?),
        )?;
        drop(guard);
        self.get_inode_from_cache_or_storage(ino).await.map(Some)
    }

    /// Count children of a directory. This **EXCLUDES** "." and "..".
    #[allow(clippy::missing_errors_doc)]
    pub fn children_count(&self, ino: u64) -> FsResult<usize> {
        if !self.is_dir(ino)? {
            return Err(FsError::InvalidInodeType);
        }
        let mut count = fs::read_dir(self.contents_path(ino).join(LS_DIR))?.count();
        if ino == ROOT_INODE {
            // we don't count "."
            count -= 1;
        } else {
            // we don't count "." and ".."
            count -= 2;
        }
        Ok(count)
    }

    #[allow(clippy::missing_panics_doc)]
    #[allow(clippy::missing_errors_doc)]
    pub async fn delete_dir(&self, parent: u64, name: &SecretString) -> FsResult<()> {
        if !self.is_dir(parent)? {
            return Err(FsError::InvalidInodeType);
        }

        if !self.exists_by_name(parent, name)? {
            return Err(FsError::NotFound("name not found"));
        }

        let attr = self
            .find_by_name(parent, name)
            .await?
            .ok_or(FsError::NotFound("name not found"))?;
        if !matches!(attr.kind, FileType::Directory) {
            return Err(FsError::InvalidInodeType);
        }
        // check if it's empty
        if self.children_count(attr.ino)? > 0 {
            return Err(FsError::NotEmpty);
        }
        let self_clone = self
            .self_weak
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        let name_clone = name.clone();
        NOD_RT
            .spawn(async move {
                // remove inode file
                {
                    let lock = self_clone
                        .serialize_inode_locks
                        .get_or_insert_with(attr.ino, || RwLock::new(false));
                    let _guard = lock.write();
                    fs::remove_file(self_clone.ino_file(attr.ino))?;
                }

                // remove contents directory
                fs::remove_dir_all(self_clone.contents_path(attr.ino))?;
                // remove from parent directory
                self_clone
                    .remove_directory_entry(parent, &name_clone)
                    .await?;
                // remove from cache
                self_clone
                    .attr_cache
                    .get()
                    .await?
                    .lock()
                    .await
                    .demote(&attr.ino);

                self_clone
                    .update_inode(
                        parent,
                        SetFileAttr::default()
                            .with_mtime(SystemTime::now())
                            .with_ctime(SystemTime::now()),
                    )
                    .await?;

                Ok(())
            })
            .await?
    }

    #[allow(clippy::missing_panics_doc)]
    #[allow(clippy::missing_errors_doc)]
    pub async fn delete_file(&self, parent: u64, name: &SecretString) -> FsResult<()> {
        if !self.is_dir(parent)? {
            return Err(FsError::InvalidInodeType);
        }
        if !self.exists_by_name(parent, name)? {
            return Err(FsError::NotFound("name not found"));
        }

        let attr = self
            .find_by_name(parent, name)
            .await?
            .ok_or(FsError::NotFound("name not found"))?;
        if !matches!(attr.kind, FileType::RegularFile) {
            return Err(FsError::InvalidInodeType);
        }
        let self_clone = self
            .self_weak
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        let name_clone = name.clone();
        NOD_RT
            .spawn(async move {
                // remove inode file
                {
                    let lock = self_clone
                        .serialize_inode_locks
                        .get_or_insert_with(attr.ino, || RwLock::new(false));
                    let _guard = lock.write();
                    fs::remove_file(self_clone.ino_file(attr.ino))?;
                }

                // remove from contents directory
                fs::remove_file(self_clone.contents_path(attr.ino))?;
                // remove from parent directory
                self_clone
                    .remove_directory_entry(parent, &name_clone)
                    .await?;
                // remove from cache
                self_clone
                    .attr_cache
                    .get()
                    .await?
                    .lock()
                    .await
                    .demote(&attr.ino);

                self_clone
                    .update_inode(
                        parent,
                        SetFileAttr::default()
                            .with_mtime(SystemTime::now())
                            .with_ctime(SystemTime::now()),
                    )
                    .await?;

                Ok(())
            })
            .await?
    }

    #[allow(clippy::missing_panics_doc)]
    #[allow(clippy::missing_errors_doc)]
    pub fn exists_by_name(&self, parent: u64, name: &SecretString) -> FsResult<bool> {
        if !self.node_exists(parent) {
            return Err(FsError::InodeNotFound);
        }
        if !self.is_dir(parent)? {
            return Err(FsError::InvalidInodeType);
        }
        let hash = hex::encode(crypto::hash_secret_string(name));
        let hash_path = self.contents_path(parent).join(HASH_DIR).join(hash);
        Ok(hash_path.is_file())
    }

    #[allow(clippy::missing_errors_doc)]
    pub async fn read_dir(&self, ino: u64, offset: u64) -> FsResult<DirectoryEntryIterator> {
        let ls_dir = self.contents_path(ino).join(LS_DIR);
        if !ls_dir.is_dir() {
            return Err(FsError::InvalidInodeType);
        }

        let iter = fs::read_dir(ls_dir)?;
        Ok(self.create_directory_entry_iterator(iter, offset).await)
    }

    /// Like [`read_dir`](EncryptedFs::read_dir) but with [`FileAttr`] so we don't need to query again for those.
    pub async fn read_dir_plus(
        &self,
        ino: u64,
        offset: u64,
    ) -> FsResult<DirectoryEntryPlusIterator> {
        let ls_dir = self.contents_path(ino).join(LS_DIR);
        if !ls_dir.is_dir() {
            return Err(FsError::InvalidInodeType);
        }

        let iter = fs::read_dir(ls_dir)?;
        Ok(self
            .create_directory_entry_plus_iterator(iter, offset)
            .await)
    }

    async fn create_directory_entry_plus(
        &self,
        entry: io::Result<DirEntry>,
    ) -> FsResult<DirectoryEntryPlus> {
        let entry = self.create_directory_entry(entry).await?;
        let lock_ino = self
            .serialize_inode_locks
            .clone()
            .get_or_insert_with(entry.ino, || RwLock::new(false));
        let _guard_ino = lock_ino.read();
        let attr = self.get_inode_from_cache_or_storage(entry.ino).await?;
        Ok(DirectoryEntryPlus {
            ino: entry.ino,
            name: entry.name,
            kind: entry.kind,
            attr,
        })
    }

    async fn create_directory_entry_plus_iterator(
        &self,
        read_dir: ReadDir,
        _offset: u64,
    ) -> DirectoryEntryPlusIterator {
        #[allow(clippy::cast_possible_truncation)]
        let futures: Vec<_> = read_dir
            .into_iter()
            .map(|entry| {
                let fs = {
                    self.self_weak
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .upgrade()
                        .unwrap()
                };
                DIR_ENTRIES_RT.spawn(async move { fs.create_directory_entry_plus(entry).await })
            })
            // .skip(offset as usize)
            .collect();

        // do these futures in parallel and return them
        let mut res = VecDeque::with_capacity(futures.len());
        for f in futures {
            res.push_back(f.await.unwrap());
        }
        DirectoryEntryPlusIterator(res)
    }

    async fn create_directory_entry(
        &self,
        entry: io::Result<DirEntry>,
    ) -> FsResult<DirectoryEntry> {
        if entry.is_err() {
            return Err(entry.err().unwrap().into());
        }
        if let Err(e) = entry {
            error!(err = %e, "reading directory entry");
            return Err(e.into());
        }
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        let name = {
            if name == "$." {
                SecretString::from_str(".").unwrap()
            } else if name == "$.." {
                SecretString::from_str("..").unwrap()
            } else {
                // try from cache
                let lock = self.get_dir_entries_name_cache().await?;
                let mut cache = lock.lock().await;
                if let Some(name_cached) = cache.get(&name).cloned() {
                    name_cached
                } else {
                    drop(cache);
                    if let Ok(decrypted_name) =
                        crypto::decrypt_file_name(&name, self.cipher, self.key.get().await?)
                            .map_err(|err| {
                                error!(err = %err, "decrypting file name");
                                err
                            })
                    {
                        lock.lock().await.put(name.clone(), decrypted_name.clone());
                        decrypted_name
                    } else {
                        return Err(FsError::InvalidInput("invalid file name"));
                    }
                }
            }
        };
        let file_path = entry.path().to_str().unwrap().to_string();
        // try from cache
        let lock = self.dir_entries_meta_cache.get().await?;
        let mut cache = lock.lock().await;
        if let Some(meta) = cache.get(&file_path) {
            return Ok(DirectoryEntry {
                ino: meta.0,
                name,
                kind: meta.1,
            });
        }
        drop(cache);
        let lock = self
            .serialize_dir_entries_ls_locks
            .get_or_insert_with(file_path.clone(), || RwLock::new(false));
        let guard = lock.read().await;
        let file = File::open(entry.path())?;
        let res: bincode::Result<(u64, FileType)> = bincode::deserialize_from(
            crypto::create_reader(file, self.cipher, self.key.get().await?),
        );
        drop(guard);
        if let Err(e) = res {
            error!(err = %e, "deserializing directory entry");
            return Err(e.into());
        }
        let (ino, kind): (u64, FileType) = res.unwrap();
        // add to cache
        self.dir_entries_meta_cache
            .get()
            .await?
            .lock()
            .await
            .put(file_path, (ino, kind));
        Ok(DirectoryEntry { ino, name, kind })
    }

    async fn get_dir_entries_name_cache(
        &self,
    ) -> FsResult<Arc<Mutex<LruCache<String, SecretString>>>> {
        self.dir_entries_name_cache.get().await
    }

    async fn create_directory_entry_iterator(
        &self,
        read_dir: ReadDir,
        _offset: u64,
    ) -> DirectoryEntryIterator {
        #[allow(clippy::cast_possible_truncation)]
        let futures: Vec<_> = read_dir
            .into_iter()
            .map(|entry| {
                let fs = {
                    self.self_weak
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .upgrade()
                        .unwrap()
                };
                DIR_ENTRIES_RT.spawn(async move { fs.create_directory_entry(entry).await })
            })
            // .skip(offset as usize)
            .collect();

        // do these futures in parallel and return them
        let mut res = VecDeque::with_capacity(futures.len());
        for f in futures {
            res.push_back(f.await.unwrap());
        }
        DirectoryEntryIterator(res)
    }

    #[allow(clippy::missing_errors_doc)]
    fn get_inode_from_storage(&self, ino: u64, key: Arc<SecretVec<u8>>) -> FsResult<FileAttr> {
        let lock = self
            .serialize_inode_locks
            .get_or_insert_with(ino, || RwLock::new(false));
        let _guard = lock.read();

        let path = self.ino_file(ino);
        if !path.is_file() {
            return Err(FsError::InodeNotFound);
        }
        let file = OpenOptions::new().read(true).open(path).map_err(|err| {
            error!(err = %err, "opening file");
            FsError::InodeNotFound
        })?;
        Ok(bincode::deserialize_from(crypto::create_reader(
            file,
            self.cipher,
            key,
        ))?)
    }

    async fn get_inode_from_cache_or_storage(&self, ino: u64) -> FsResult<FileAttr> {
        let lock = self.attr_cache.get().await?;
        let mut guard = lock.lock().await;
        let attr = guard.get(&ino);
        if let Some(attr) = attr {
            Ok(*attr)
        } else {
            let attr = self.get_inode_from_storage(ino, self.key.get().await?)?;
            guard.put(ino, attr);
            Ok(attr)
        }
    }

    #[allow(clippy::missing_errors_doc)]
    pub async fn get_inode(&self, ino: u64) -> FsResult<FileAttr> {
        let mut attr = self.get_inode_from_cache_or_storage(ino).await?;

        // merge time info with any open read handles
        let open_reads = { self.opened_files_for_read.read().await.contains_key(&ino) };
        if open_reads {
            let fhs = self.opened_files_for_read.read().await.get(&ino).cloned();
            if let Some(fhs) = fhs {
                for fh in fhs {
                    if let Some(ctx) = self.read_handles.read().await.get(&fh) {
                        let mut attr1: SetFileAttr = ctx.lock().await.attr.clone().into();
                        // we don't want to set size because readers don't change the size, and we might have an older version
                        attr1.size.take();
                        merge_attr(&mut attr, &attr1);
                    }
                }
            }
        }

        // merge time info and size with any open write handles
        let open_writes = { self.opened_files_for_write.read().await.contains_key(&ino) };
        if open_writes {
            let fh = self.opened_files_for_write.read().await.get(&ino).copied();
            if let Some(fh) = fh {
                if let Some(ctx) = self.write_handles.read().await.get(&fh) {
                    let ctx = ctx.lock().await;
                    merge_attr(&mut attr, &ctx.attr.clone().into());
                }
            }
        }

        Ok(attr)
    }

    pub async fn update_inode(&self, ino: u64, set_attr: SetFileAttr) -> FsResult<()> {
        let lock_serialize_update = self
            .serialize_update_inode_locks
            .get_or_insert_with(ino, || Mutex::new(false));
        let _guard_serialize_update = lock_serialize_update.lock().await;

        let mut attr = self.get_inode(ino).await?;
        merge_attr(&mut attr, &set_attr);

        self.write_inode_to_storage(&attr).await?;
        Ok(())
    }

    async fn write_inode_to_storage(&self, attr: &FileAttr) -> Result<(), FsError> {
        let lock = self
            .serialize_inode_locks
            .get_or_insert_with(attr.ino, || RwLock::new(false));
        let guard = lock.write().await;
        crypto::atomic_serialize_encrypt_into(
            &self.ino_file(attr.ino),
            attr,
            self.cipher,
            self.key.get().await?,
        )?;
        drop(guard);
        // update cache also
        {
            let lock = self.attr_cache.get().await?;
            let mut guard = lock.lock().await;
            guard.put(attr.ino, *attr);
        }
        Ok(())
    }

    /// Read the contents from an 'offset'. If we try to read outside of file size, we return 0 bytes.
    /// Depending on the encryption type we might need to re-read bytes until the 'offset', in some case even
    /// from the beginning of the file to the desired `offset`. This will slow down the read operation if we
    /// read from very distanced offsets.
    /// The most speed is obtained when we read sequentially from the beginning of the file.
    /// If the file is not opened for read, it will return an error of type ['FsError::InvalidFileHandle'].
    #[instrument(skip(self, buf))]
    #[allow(clippy::missing_errors_doc)]
    #[allow(clippy::cast_possible_truncation)]
    pub async fn read(
        &self,
        ino: u64,
        offset: u64,
        buf: &mut [u8],
        handle: u64,
    ) -> FsResult<usize> {
        if !self.node_exists(ino) {
            return Err(FsError::InodeNotFound);
        }
        if !self.is_file(ino)? {
            return Err(FsError::InvalidInodeType);
        }
        if !self.read_handles.read().await.contains_key(&handle) {
            return Err(FsError::InvalidFileHandle);
        }

        let guard = self.read_handles.read().await;
        let mut ctx = guard.get(&handle).unwrap().lock().await;

        if ctx.ino != ino {
            return Err(FsError::InvalidFileHandle);
        }
        if self.is_dir(ino)? {
            return Err(FsError::InvalidInodeType);
        }
        if buf.is_empty() {
            // no-op
            return Ok(0);
        }

        // read data
        let len = {
            let reader = ctx.reader.as_mut().unwrap();

            reader.seek(SeekFrom::Start(offset)).map_err(|err| {
                error!(err = %err, "seeking");
                err
            })?;
            let pos = reader.stream_position().map_err(|err| {
                error!(err = %err, "getting position");
                err
            })?;
            if pos != offset {
                // we would need to seek after filesize
                return Ok(0);
            }
            stream_util::read(reader, buf).map_err(|err| {
                error!(err = %err, "reading");
                err
            })?
        };

        ctx.attr.atime = SystemTime::now();
        drop(ctx);

        Ok(len)
    }

    #[allow(clippy::missing_panics_doc)]
    pub async fn release(&self, handle: u64) -> FsResult<()> {
        if handle == 0 {
            // in case of directory or if the file was crated without being opened we don't use handle
            return Ok(());
        }
        let mut valid_fh = false;

        // read
        let ctx = { self.read_handles.write().await.remove(&handle) };
        if let Some(ctx) = ctx {
            let ctx = ctx.lock().await;

            {
                let mut opened_files_for_read = self.opened_files_for_read.write().await;
                opened_files_for_read
                    .get_mut(&ctx.ino)
                    .expect("handle is missing")
                    .remove(&handle);
                if opened_files_for_read
                    .get(&ctx.ino)
                    .expect("handle is missing")
                    .is_empty()
                {
                    opened_files_for_read.remove(&ctx.ino);
                }
            }

            // write attr only here to avoid serializing it multiple times while reading
            // it will merge time fields with existing data because it might got change while we kept the handle
            let mut attr: SetFileAttr = ctx.attr.clone().into();
            // we don't want to set size because readers don't change the size, and we might have an older version
            attr.size.take();
            self.update_inode(ctx.ino, attr).await?;

            drop(ctx);
            valid_fh = true;
        }

        // write
        let ctx = { self.write_handles.write().await.remove(&handle) };
        if let Some(ctx) = ctx {
            let mut ctx = ctx.lock().await;

            let mut writer = ctx.writer.take().unwrap();
            writer.flush()?;
            writer.finish()?;

            self.opened_files_for_write.write().await.remove(&ctx.ino);

            // write attr only here to avoid serializing it multiple times while writing
            // it will merge time fields with existing data because it might got change while we kept the handle
            self.update_inode(ctx.ino, ctx.attr.clone().into()).await?;
            drop(ctx);

            valid_fh = true;
        }

        if !valid_fh {
            return Err(FsError::InvalidFileHandle);
        }
        Ok(())
    }

    /// Check if a file is opened for read with this handle.
    pub async fn is_read_handle(&self, fh: u64) -> bool {
        self.read_handles.read().await.contains_key(&fh)
    }

    /// Check if a file is opened for write with this handle.
    pub async fn is_write_handle(&self, fh: u64) -> bool {
        self.write_handles.read().await.contains_key(&fh)
    }

    /// Writes the contents of `buf` to the file at `ino` starting at `offset`.  
    /// Depending on the encryption type we might need to re-write bytes until the 'offset', in some case even
    /// from the beginning of the file to the desired `offset`. This will slow down the write operation if we
    /// write to very distanced offsets.  
    /// The most speed is obtained when we write sequentially from the beginning of the file.  
    /// If we write outside of file size, we fill up with zeros until offset.  
    /// If the file is not opened for write, it will return an error of type ['FsError::InvalidFileHandle'].
    #[instrument(skip(self, buf))]
    pub async fn write(&self, ino: u64, offset: u64, buf: &[u8], handle: u64) -> FsResult<usize> {
        if !self.node_exists(ino) {
            return Err(FsError::InodeNotFound);
        }
        if !self.is_file(ino)? {
            return Err(FsError::InvalidInodeType);
        }
        {
            if !self.write_handles.read().await.contains_key(&handle) {
                return Err(FsError::InvalidFileHandle);
            }
        }
        {
            let guard = self.write_handles.read().await;
            let ctx = guard.get(&handle).unwrap().lock().await;
            if ctx.ino != ino {
                return Err(FsError::InvalidFileHandle);
            }
        }
        if buf.is_empty() {
            // no-op
            return Ok(0);
        }

        let guard = self.write_handles.read().await;
        let mut ctx = guard.get(&handle).unwrap().lock().await;

        // write new data
        let (pos, len) = {
            if offset > self.cipher.max_plaintext_len() as u64 {
                return Err(FsError::MaxFilesizeExceeded(
                    self.cipher.max_plaintext_len(),
                ));
            }
            let writer = ctx.writer.as_mut().unwrap();
            let pos = writer.seek(SeekFrom::Start(offset)).map_err(|err| {
                error!(err = %err, "seeking");
                err
            })?;
            if offset != pos {
                // we could not seek to the desired position
                return Ok(0);
            }
            // keep filesize to max the cipher can handle
            #[allow(clippy::cast_possible_truncation)]
            let buf = if offset + buf.len() as u64 > self.cipher.max_plaintext_len() as u64 {
                &buf[..(self.cipher.max_plaintext_len() - offset as usize)]
            } else {
                buf
            };
            let len = writer.write(buf).map_err(|err| {
                error!(err = %err, "writing");
                err
            })?;
            (writer.stream_position()?, len)
        };

        if pos > ctx.attr.size {
            // if we write pass file size set the new size
            debug!("setting new file size {}", pos);
            ctx.attr.size = pos;
        }
        ctx.attr.mtime = SystemTime::now();
        ctx.attr.ctime = SystemTime::now();
        drop(ctx);

        Ok(len)
    }

    /// Flush the data to the underlying storage.
    #[allow(clippy::missing_panics_doc)]
    pub async fn flush(&self, handle: u64) -> FsResult<()> {
        if handle == 0 {
            // in case of directory or if the file was crated without being opened we don't use handle
            return Ok(());
        }
        let mut valid_fh = self.read_handles.read().await.get(&handle).is_some();
        if let Some(ctx) = self.write_handles.read().await.get(&handle) {
            ctx.lock()
                .await
                .writer
                .as_mut()
                .expect("writer is missing")
                .flush()?;
            valid_fh = true;
        }

        if !valid_fh {
            return Err(FsError::InvalidFileHandle);
        }

        Ok(())
    }

    /// Helpful when we want to copy just some portions of the file.
    pub async fn copy_file_range(
        &self,
        src_ino: u64,
        src_offset: u64,
        dest_ino: u64,
        dest_offset: u64,
        size: usize,
        src_fh: u64,
        dest_fh: u64,
    ) -> FsResult<usize> {
        if self.is_dir(src_ino)? || self.is_dir(dest_ino)? {
            return Err(FsError::InvalidInodeType);
        }

        let mut buf = vec![0; size];
        let len = self.read(src_ino, src_offset, &mut buf, src_fh).await?;
        if len == 0 {
            return Ok(0);
        }
        let mut copied = 0;
        while copied < size {
            let len = self
                .write(dest_ino, dest_offset, &buf[copied..len], dest_fh)
                .await?;
            if len == 0 && copied < size {
                error!(len, "Failed to copy all read bytes");
                return Err(FsError::Other("Failed to copy all read bytes"));
            }
            copied += len;
        }
        Ok(len)
    }

    /// Open a file. We can open multiple times for read but only one for write at a time.
    #[allow(clippy::missing_panics_doc)]
    pub async fn open(&self, ino: u64, read: bool, write: bool) -> FsResult<u64> {
        if !read && !write {
            return Err(FsError::InvalidInput(
                "read and write cannot be false at the same time",
            ));
        }
        if self.is_dir(ino)? {
            return Err(FsError::InvalidInodeType);
        }

        let mut handle: Option<u64> = None;
        if read {
            handle = Some(self.allocate_next_handle());
            self.do_with_read_handle(
                *handle.as_ref().unwrap(),
                ReadHandleContextOperation::Create { ino },
            )
            .await?;
        }
        if write {
            if handle.is_none() {
                handle = Some(self.allocate_next_handle());
            }
            let res = self
                .do_with_write_handle(
                    *handle.as_ref().expect("handle is missing"),
                    WriteHandleContextOperation::Create { ino },
                )
                .await;
            if res.is_err() && read {
                // on error remove the read handle if it was added above
                // remove the read handle if it was added above
                self.read_handles
                    .write()
                    .await
                    .remove(handle.as_ref().unwrap());
                return Err(FsError::AlreadyOpenForWrite);
            }
            res?;
        }
        Ok(handle.unwrap())
    }

    #[allow(clippy::missing_panics_doc)]
    #[allow(clippy::too_many_lines)]
    pub async fn truncate(&self, ino: u64, size: u64) -> FsResult<()> {
        let attr = self.get_inode(ino).await?;
        if matches!(attr.kind, FileType::Directory) {
            return Err(FsError::InvalidInodeType);
        }

        if size == attr.size {
            // no-op
            return Ok(());
        }

        // flush writers
        self.flush_and_reset_writers(ino).await?;

        let lock = self
            .read_write_locks
            .get_or_insert_with(ino, || RwLock::new(false));
        // obtain a write lock to whole file, we ue a special value `u64::MAX - 42_u64` to indicate this
        let _guard = lock.write().await;

        if size == 0 {
            debug!("truncate to zero");
            // truncate to zero
            let file_path = self.contents_path(ino);
            fs::remove_file(&file_path)?;
            File::create_new(&file_path)?;
        } else {
            debug!("truncate size to {}", size.to_formatted_string(&Locale::en));

            let in_path = self.contents_path(attr.ino);
            let mut file = fs_util::open_atomic_write(&self.contents_path(ino))?;
            {
                // have a new scope, so we drop the reader before moving new content files
                let mut reader = self.create_file_reader(&in_path, None).await?;

                let mut writer = self.create_writer(file).await?;

                let len = if size > attr.size {
                    // increase size, copy existing data until existing size
                    attr.size
                } else {
                    // decrease size, copy existing data until new size
                    size
                };
                stream_util::copy_exact(&mut reader, &mut writer, len)?;
                if size > attr.size {
                    // increase size, seek to new size will write zeros
                    stream_util::fill_zeros(&mut writer, size - attr.size)?;
                }
                writer.flush()?;
                file = writer.finish()?;
            }
            file.commit()?;
        }

        let set_attr = SetFileAttr::default()
            .with_size(size)
            .with_mtime(SystemTime::now())
            .with_ctime(SystemTime::now());
        self.update_inode(ino, set_attr).await?;

        // also recreate handles because the file has changed
        self.reset_handles(attr.ino, 0, None).await?;

        Ok(())
    }

    /// This will write any dirty data to the file and seek to start
    /// > ⚠️ **Warning**
    /// > Need to be called in a context with write lock on `self.read_write_inode_locks.lock().await.get(ino)`.
    async fn flush_and_reset_writers(&self, ino: u64) -> FsResult<()> {
        let map = self.opened_files_for_write.read().await;
        let handle = map.get(&ino);
        if let Some(handle) = handle {
            let guard = self.write_handles.read().await;
            if let Some(lock) = guard.get(handle) {
                let mut ctx = lock.lock().await;
                let writer = ctx.writer.as_mut().unwrap();
                writer.flush()?;
                writer.seek(SeekFrom::Start(0))?;
            }
        }

        Ok(())
    }

    #[allow(clippy::missing_panics_doc)]
    pub async fn rename(
        &self,
        parent: u64,
        name: &SecretString,
        new_parent: u64,
        new_name: &SecretString,
    ) -> FsResult<()> {
        if !self.node_exists(parent) {
            return Err(FsError::InodeNotFound);
        }
        if !self.is_dir(parent)? {
            return Err(FsError::InvalidInodeType);
        }
        if !self.node_exists(new_parent) {
            return Err(FsError::InodeNotFound);
        }
        if !self.is_dir(new_parent)? {
            return Err(FsError::InvalidInodeType);
        }
        if !self.exists_by_name(parent, name)? {
            return Err(FsError::NotFound("name not found"));
        }

        if parent == new_parent && name.expose_secret() == new_name.expose_secret() {
            // no-op
            return Ok(());
        }

        // Only overwrite an existing directory if it's empty
        if let Ok(Some(new_attr)) = self.find_by_name(new_parent, new_name).await {
            if new_attr.kind == FileType::Directory && self.children_count(new_attr.ino)? > 0 {
                return Err(FsError::NotEmpty);
            }
        }

        let mut attr = self
            .find_by_name(parent, name)
            .await?
            .ok_or(FsError::NotFound("name not found"))?;
        // remove from parent contents
        self.remove_directory_entry(parent, name).await?;
        // add to new parent contents
        self.insert_directory_entry(
            new_parent,
            &DirectoryEntry {
                ino: attr.ino,
                name: new_name.clone(),
                kind: attr.kind,
            },
        )
        .await?;

        let mut parent_attr = self.get_inode(parent).await?;
        parent_attr.mtime = SystemTime::now();
        parent_attr.ctime = SystemTime::now();

        let mut new_parent_attr = self.get_inode(new_parent).await?;
        new_parent_attr.mtime = SystemTime::now();
        new_parent_attr.ctime = SystemTime::now();

        attr.ctime = SystemTime::now();

        if attr.kind == FileType::Directory {
            // add parent link to new directory
            self.insert_directory_entry(
                attr.ino,
                &DirectoryEntry {
                    ino: new_parent,
                    name: SecretString::from_str("$..").expect("cannot parse"),
                    kind: FileType::Directory,
                },
            )
            .await?;
        }

        Ok(())
    }

    /// Create a crypto writer using internal encryption info.
    pub async fn create_writer<W: Write + Send + Sync>(
        &self,
        file: W,
    ) -> FsResult<impl CryptoWriter<W>> {
        Ok(crypto::create_writer(
            file,
            self.cipher,
            self.key.get().await?,
        ))
    }

    /// Create a crypto writer to file using internal encryption info.
    ///
    /// **`callback`** is called when the file content changes. It receives the position from where the file content changed and the last write position
    ///
    /// **`lock`** is used to write lock the file when accessing it. If not provided, it will not ensure that other instances are not writing to the file while we do  
    ///     You need to provide the same lock to all writers and readers of this file, you should obtain a new [`Holder`] that wraps the same lock
    ///
    /// **`metadata_provider`** it's used to do some optimizations to reduce some copy operations from original file  
    ///     If the file exists or is created before flushing, in worse case scenarios, it can reduce the overall write speed by half, so it's recommended to provide it
    pub async fn create_file_writer(
        &self,
        file: &Path,
        callback: Option<Box<dyn FileCryptoWriterCallback>>,
        lock: Option<Holder<RwLock<bool>>>,
        metadata_provider: Option<Box<dyn FileCryptoWriterMetadataProvider>>,
    ) -> FsResult<Box<dyn CryptoWriterSeek<File>>> {
        Ok(crypto::create_file_writer(
            file,
            self.cipher,
            self.key.get().await?,
            *Box::new(callback),
            lock,
            metadata_provider,
        )?)
    }

    /// Create a crypto reader from file using internal encryption info.
    /// **`lock`** is used to read lock the file when accessing it. If not provided, it will not ensure that other instances are not writing to the file while we read\
    ///     You need to provide the same lock to any writers to this file, you should obtain a new [`Holder`] that wraps the same lock,
    pub async fn create_file_reader(
        &self,
        file: &Path,
        lock: Option<Holder<RwLock<bool>>>,
    ) -> FsResult<Box<dyn CryptoReader>> {
        Ok(crypto::create_file_reader(
            file,
            self.cipher,
            self.key.get().await?,
            lock,
        )?)
    }

    /// Create a crypto reader using internal encryption info.
    pub async fn create_reader(&self, file: File) -> FsResult<impl CryptoReader> {
        Ok(crypto::create_reader(
            file,
            self.cipher,
            self.key.get().await?,
        ))
    }

    /// Decrypts a string using internal encryption info.
    pub async fn decrypt_string(&self, s: &str) -> FsResult<SecretString> {
        Ok(crypto::decrypt_string(
            s,
            self.cipher,
            self.key.get().await?,
        )?)
    }

    /// Change the password of the filesystem used to access the encryption key.
    pub async fn change_password(
        data_dir: &Path,
        old_password: SecretString,
        new_password: SecretString,
        cipher: Cipher,
    ) -> FsResult<()> {
        check_structure(data_dir, false).await?;
        // decrypt key
        let salt: Vec<u8> = bincode::deserialize_from(File::open(
            data_dir.join(SECURITY_DIR).join(KEY_SALT_FILENAME),
        )?)?;
        let initial_key = crypto::derive_key(&old_password, cipher, &salt)?;
        let enc_file = data_dir.join(SECURITY_DIR).join(KEY_ENC_FILENAME);
        let reader = crypto::create_reader(File::open(enc_file)?, cipher, Arc::new(initial_key));
        let key: Vec<u8> =
            bincode::deserialize_from(reader).map_err(|_| FsError::InvalidPassword)?;
        let key = SecretVec::new(key);
        // encrypt it with new key derived from new password
        let new_key = crypto::derive_key(&new_password, cipher, &salt)?;
        crypto::atomic_serialize_encrypt_into(
            &data_dir.join(SECURITY_DIR).join(KEY_ENC_FILENAME),
            &key.expose_secret(),
            cipher,
            Arc::new(new_key),
        )?;
        Ok(())
    }

    fn allocate_next_handle(&self) -> u64 {
        self.current_handle
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) async fn reset_handles(
        &self,
        ino: u64,
        pos: i64,
        skip_fh: Option<u64>,
    ) -> FsResult<()> {
        // read
        if let Some(set) = self.opened_files_for_read.read().await.get(&ino) {
            for handle in set.iter().filter(|h| skip_fh.map_or(true, |fh| **h != fh)) {
                let map = self.read_handles.read().await;
                let mut ctx = map.get(handle).unwrap().lock().await;
                let reader = ctx.reader.as_mut().unwrap();
                #[allow(clippy::cast_possible_wrap)]
                if reader.stream_position()? as i64 > pos {
                    reader.seek(SeekFrom::Start(0))?;
                }
            }
        }

        // write
        if let Some(fh) = self.opened_files_for_write.read().await.get(&ino) {
            if let Some(handle) = skip_fh {
                if *fh == handle {
                    return Ok(());
                }
            }
            if let Some(ctx) = self.write_handles.read().await.get(fh) {
                let mut ctx = ctx.lock().await;
                let writer = ctx.writer.as_mut().unwrap();
                writer.flush()?;
                writer.seek(SeekFrom::Start(0))?;
                let attr = self.get_inode_from_storage(ino, self.key.get().await?)?;
                ctx.attr.size = attr.size;
            }
        }

        Ok(())
    }

    async fn do_with_read_handle(
        &self,
        handle: u64,
        op: ReadHandleContextOperation,
    ) -> FsResult<()> {
        let ino = op.get_ino();
        let path = self.contents_path(ino);
        let attr = self.get_inode_from_storage(ino, self.key.get().await?)?;
        match op {
            ReadHandleContextOperation::Create { ino } => {
                let attr: TimeAndSizeFileAttr = attr.into();
                let lock = self
                    .read_write_locks
                    .get_or_insert_with(ino, || RwLock::new(false));
                let reader = self.create_file_reader(&path, Some(lock)).await?;
                let ctx = ReadHandleContext {
                    ino,
                    attr,
                    reader: Some(reader),
                };
                self.read_handles
                    .write()
                    .await
                    .insert(handle, Mutex::new(ctx));
                self.opened_files_for_read
                    .write()
                    .await
                    .entry(ino)
                    .or_insert_with(|| HashSet::new())
                    .insert(handle);
            }
        }
        Ok(())
    }

    async fn do_with_write_handle(
        &self,
        handle: u64,
        op: WriteHandleContextOperation,
    ) -> FsResult<()> {
        let ino = op.get_ino();
        let path = self.contents_path(ino);
        let callback = LocalFileCryptoWriterCallback(
            (*self.self_weak.lock().unwrap().as_ref().unwrap()).clone(),
            ino,
            handle,
        );
        match op {
            WriteHandleContextOperation::Create { ino } => {
                let attr = self.get_inode(ino).await?.into();
                let metadata_provider = Box::new(LocalFileCryptoWriterMetadataProvider(
                    (*self.self_weak.lock().unwrap().as_ref().unwrap()).clone(),
                    ino,
                ));
                let lock = self
                    .read_write_locks
                    .get_or_insert_with(ino, || RwLock::new(false));
                let writer = self
                    .create_file_writer(
                        &path,
                        Some(Box::new(callback)),
                        Some(lock),
                        Some(metadata_provider),
                    )
                    .await?;
                let ctx = WriteHandleContext {
                    ino,
                    attr,
                    writer: Some(writer),
                };
                self.write_handles
                    .write()
                    .await
                    .insert(handle, Mutex::new(ctx));
                self.opened_files_for_write
                    .write()
                    .await
                    .insert(ino, handle);
            }
        }

        Ok(())
    }

    async fn ensure_root_exists(&self) -> FsResult<()> {
        if !self.node_exists(ROOT_INODE) {
            let mut attr: FileAttr = CreateFileAttr {
                kind: FileType::Directory,
                perm: 0o755,
                uid: 0,
                gid: 0,
                rdev: 0,
                flags: 0,
            }
            .into();
            attr.ino = ROOT_INODE;
            #[cfg(target_os = "linux")]
            unsafe {
                attr.uid = libc::getuid();
                attr.gid = libc::getgid();
            }

            self.write_inode_to_storage(&attr).await?;

            // create in contents directory
            fs::create_dir(self.contents_path(attr.ino))?;
            fs::create_dir(self.contents_path(attr.ino).join(LS_DIR))?;
            fs::create_dir(self.contents_path(attr.ino).join(HASH_DIR))?;

            // add "." entry
            self.insert_directory_entry(
                attr.ino,
                &DirectoryEntry {
                    ino: attr.ino,
                    name: SecretString::from_str("$.").unwrap(),
                    kind: FileType::Directory,
                },
            )
            .await?;
        }

        Ok(())
    }

    async fn insert_directory_entry(
        &self,
        ino_contents_dir: u64,
        entry: &DirectoryEntry,
    ) -> FsResult<()> {
        let parent_path = self.contents_path(ino_contents_dir);
        let encrypted_name =
            crypto::encrypt_file_name(&entry.name, self.cipher, self.key.get().await?)?;
        // add to LS directory
        let self_clone = self
            .self_weak
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        let parent_path_clone = parent_path.clone();
        let encrypted_name_clone = encrypted_name.clone();
        let entry_clone = entry.clone();
        // spawn a task to do concurrently with adding to HASH directory
        let h = tokio::spawn(async move {
            let file_path = parent_path_clone
                .join(LS_DIR)
                .join(encrypted_name_clone.clone());
            let lock = self_clone
                .serialize_dir_entries_ls_locks
                .get_or_insert_with(file_path.to_str().unwrap().to_string(), || {
                    RwLock::new(false)
                });
            let _guard = lock.write().await;
            // write inode and file type
            let entry = (entry_clone.ino, entry_clone.kind);
            crypto::atomic_serialize_encrypt_into(
                &file_path,
                &entry,
                self_clone.cipher,
                self_clone.key.get().await?,
            )?;
            Ok::<(), FsError>(())
        });
        // add to HASH directory
        let self_clone = self
            .self_weak
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        let entry_hash = entry.clone();
        tokio::spawn(async move {
            let name = crypto::hash_file_name(&entry_hash.name)?;
            let file_path = parent_path.join(HASH_DIR).join(name);
            let lock = self_clone
                .serialize_dir_entries_hash_locks
                .get_or_insert_with(file_path.to_str().unwrap().to_string(), || {
                    RwLock::new(false)
                });
            let _guard = lock.write().await;
            // write inode and file type
            // we save the encrypted name also because we need it to remove the entry on [`remove_directory_entry`]
            let entry = (entry_hash.ino, entry_hash.kind, encrypted_name);
            crypto::atomic_serialize_encrypt_into(
                &file_path,
                &entry,
                self_clone.cipher,
                self_clone.key.get().await?,
            )?;
            Ok::<(), FsError>(())
        })
        .await??;
        h.await??;
        Ok(())
    }

    fn ino_file(&self, ino: u64) -> PathBuf {
        self.data_dir.join(INODES_DIR).join(ino.to_string())
    }

    fn contents_path(&self, ino: u64) -> PathBuf {
        self.data_dir.join(CONTENTS_DIR).join(ino.to_string())
    }

    async fn remove_directory_entry(&self, parent: u64, name: &SecretString) -> FsResult<()> {
        let parent_path = self.contents_path(parent);
        // remove from HASH
        let name = crypto::hash_file_name(name)?;
        let path = parent_path.join(HASH_DIR).join(name);
        let lock = self
            .serialize_dir_entries_hash_locks
            .get_or_insert_with(path.to_str().unwrap().to_string(), || RwLock::new(false));
        let guard = lock.write().await;
        let (_, _, name): (u64, FileType, String) =
            bincode::deserialize_from(crypto::create_reader(
                File::open(path.clone())?,
                self.cipher,
                self.key.get().await?,
            ))?;
        fs::remove_file(path)?;
        drop(guard);
        // remove from LS
        let path = parent_path.join(LS_DIR).join(name);
        let lock = self
            .serialize_dir_entries_ls_locks
            .get_or_insert_with(path.to_str().unwrap().to_string(), || RwLock::new(false));
        let _guard = lock.write().await;
        fs::remove_file(path)?;
        Ok(())
    }

    fn generate_next_inode(&self) -> u64 {
        loop {
            let ino = crypto::create_rng().next_u64();

            if ino <= ROOT_INODE {
                continue;
            }
            if self.node_exists(ino) {
                continue;
            }

            return ino;
        }
    }
}

fn read_or_create_key(
    key_path: &PathBuf,
    salt_path: &PathBuf,
    password: &SecretString,
    cipher: Cipher,
) -> FsResult<SecretVec<u8>> {
    let salt = if salt_path.exists() {
        bincode::deserialize_from(File::open(salt_path)?).map_err(|_| FsError::InvalidPassword)?
    } else {
        let mut salt = vec![0; 16];
        crypto::create_rng().fill_bytes(&mut salt);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(salt_path)?;
        bincode::serialize_into(&mut file, &salt)?;
        file.flush()?;
        file.sync_all()?;
        File::open(salt_path.parent().expect("oops, we don't have a parent"))?.sync_all()?;
        salt
    };
    // derive key from password
    let derived_key = crypto::derive_key(password, cipher, &salt)?;
    if key_path.exists() {
        // read key
        let reader = crypto::create_reader(File::open(key_path)?, cipher, Arc::new(derived_key));
        let key: Vec<u8> =
            bincode::deserialize_from(reader).map_err(|_| FsError::InvalidPassword)?;
        Ok(SecretVec::new(key))
    } else {
        // first time, create a random key and encrypt it with the derived key from password
        let mut key: Vec<u8> = vec![];
        let key_len = cipher.key_len();
        key.resize(key_len, 0);
        crypto::create_rng().fill_bytes(&mut key);
        let mut writer = crypto::create_writer(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(key_path)?,
            cipher,
            Arc::new(derived_key),
        );
        bincode::serialize_into(&mut writer, &key)?;
        writer.flush()?;
        writer.finish()?;
        Ok(SecretVec::new(key))
    }
}

async fn ensure_structure_created(data_dir: &PathBuf) -> FsResult<()> {
    if data_dir.exists() {
        check_structure(data_dir, true).await?;
    } else {
        fs::create_dir_all(data_dir)?;
    }

    // create directories
    let dirs = vec![INODES_DIR, CONTENTS_DIR, SECURITY_DIR];
    for dir in dirs {
        let path = data_dir.join(dir);
        if !path.exists() {
            fs::create_dir_all(path)?;
        }
    }

    Ok(())
}

async fn check_structure(data_dir: &Path, ignore_empty: bool) -> FsResult<()> {
    if !data_dir.exists() || !data_dir.is_dir() {
        return Err(FsError::InvalidDataDirStructure);
    }
    let mut vec = ReadDirStream::new(tokio::fs::read_dir(data_dir).await?)
        .try_collect::<Vec<_>>()
        .await?
        .iter()
        .map(|dir| dir.file_name().to_string_lossy().to_string())
        .collect::<Vec<String>>();
    if vec.is_empty() && ignore_empty {
        return Ok(());
    }
    if vec.len() != 3 {
        return Err(FsError::InvalidDataDirStructure);
    }
    // make sure existing structure is ok
    vec.sort_unstable();
    let mut vec2 = vec![INODES_DIR, CONTENTS_DIR, SECURITY_DIR];
    vec2.sort_unstable();
    if vec != vec2
        || !data_dir.join(SECURITY_DIR).join(KEY_ENC_FILENAME).is_file()
        || !data_dir
            .join(SECURITY_DIR)
            .join(KEY_SALT_FILENAME)
            .is_file()
    {
        return Err(FsError::InvalidDataDirStructure);
    }

    Ok(())
}

fn merge_attr(attr: &mut FileAttr, set_attr: &SetFileAttr) {
    if let Some(size) = set_attr.size {
        attr.size = size;
    }
    if let Some(atime) = set_attr.atime {
        attr.atime = max(atime, attr.atime);
    }
    if let Some(mtime) = set_attr.mtime {
        attr.mtime = max(mtime, attr.mtime);
    }
    if let Some(ctime) = set_attr.ctime {
        attr.ctime = max(ctime, attr.ctime);
    }
    if let Some(crtime) = set_attr.crtime {
        attr.crtime = max(crtime, attr.crtime);
    }
    if let Some(perm) = set_attr.perm {
        attr.perm = perm;
    }
    if let Some(uid) = set_attr.uid {
        attr.uid = uid;
    }
    if let Some(gid) = set_attr.gid {
        attr.gid = gid;
    }
    if let Some(flags) = set_attr.flags {
        attr.flags = flags;
    }
}

struct LocalFileCryptoWriterCallback(Weak<EncryptedFs>, u64, u64);

impl FileCryptoWriterCallback for LocalFileCryptoWriterCallback {
    #[instrument(skip(self))]
    fn on_file_content_changed(
        &self,
        changed_from_pos: i64,
        last_write_pos: u64,
    ) -> io::Result<()> {
        debug!("on file content changed");
        call_async(async {
            if let Some(fs) = self.0.upgrade() {
                reset_handles(fs, self.1, changed_from_pos, last_write_pos, self.2)
                    .await
                    .map_err(|e| io::Error::new(Other, e))?;
                Ok(())
            } else {
                Err(io::Error::new(Other, "fs dropped"))
            }
        })
    }
}

pub async fn write_all_string_to_fs(
    fs: &EncryptedFs,
    ino: u64,
    offset: u64,
    s: &str,
    fh: u64,
) -> FsResult<()> {
    write_all_bytes_to_fs(fs, ino, offset, s.as_bytes(), fh).await
}

#[allow(clippy::missing_panics_doc)]
pub async fn write_all_bytes_to_fs(
    fs: &EncryptedFs,
    ino: u64,
    offset: u64,
    buf: &[u8],
    fh: u64,
) -> FsResult<()> {
    let mut pos = 0_usize;
    loop {
        let len = fs.write(ino, offset, &buf[pos..], fh).await?;
        pos += len;
        if pos == buf.len() {
            break;
        } else if len == 0 {
            return Err(FsError::Other("Failed to write all bytes"));
        }
    }
    fs.flush(fh).await?;
    Ok(())
}
