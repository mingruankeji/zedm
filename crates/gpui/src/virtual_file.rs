//! Deferred file content for native drag destinations. Payload resolution performs no IO.

use smallvec::SmallVec;
use std::{
    ffi::OsString,
    fmt, io,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

/// Maximum content request; adapters split larger reads into bounded chunks.
pub const VIRTUAL_FILE_CHUNK_SIZE: usize = 64 * 1024;

/// Native drag status describes delivery to the consumer, not final disk persistence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeFileDragEvent {
    /// The native gesture has started.
    Started,
    /// A consumer accepted the drop.
    Dropped,
    /// Terminal delivery status, emitted once per gesture.
    Finished(NativeFileDragOutcome),
}

/// Final native consumer status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeFileDragOutcome {
    /// The consumer released or completed its content operation.
    Provided,
    /// The source or consumer cancelled.
    Cancelled,
    /// Native delivery or a source read failed.
    Failed,
}

/// Optional worker-safe observer shared by flat and deferred virtual payloads.
pub trait NativeFileDragObserver: Send + Sync + 'static {
    /// May run on a native worker; must not block or access UI entities.
    fn observe(&self, event: NativeFileDragEvent);
    /// User pauses are excluded from native idle expiry.
    fn is_paused(&self) -> bool {
        false
    }
}

#[derive(Clone, Default)]
struct DragObserver(Option<Arc<dyn NativeFileDragObserver>>);
impl fmt::Debug for DragObserver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DragObserver")
            .field(&self.0.is_some())
            .finish()
    }
}
impl PartialEq for DragObserver {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }
}
impl Eq for DragObserver {}

/// A reusable deferred source, independent of the application's transport.
pub trait VirtualFileProvider: Send + Sync + 'static {
    /// Create an independent stream without blocking IO. Platform workers request content.
    fn open(&self) -> io::Result<Box<dyn VirtualFileStream>>;
    /// Cancel every open stream and prevent subsequent opens. Must not block.
    fn cancel(&self);
}

/// A seekable content handle. Dropping it must cancel outstanding work.
pub trait VirtualFileStream: Send + 'static {
    /// Read at an explicit offset, returning zero only at EOF. The buffer is at
    /// most `VIRTUAL_FILE_CHUNK_SIZE` bytes. Propagate cancellation and IO failures.
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> io::Result<usize>;
    /// Wake pending requests. Must not block.
    fn cancel(&self);
}

/// Metadata advertised before a destination requests content.
#[derive(Clone)]
pub struct VirtualFileDescriptor {
    /// A safe relative name (backslash-separated for trees), never a destination.
    pub name: OsString,
    /// Directories have metadata only; content indexes are reserved for files.
    pub is_directory: bool,
    /// Optional advertised length.
    pub size: Option<u64>,
    /// Optional last modification time.
    pub modified_at: Option<SystemTime>,
    /// Deferred content provider; GPUI knows nothing about its transport.
    pub provider: Arc<dyn VirtualFileProvider>,
}

impl fmt::Debug for VirtualFileDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualFileDescriptor")
            .field("name", &self.name)
            .field("size", &self.size)
            .field("modified_at", &self.modified_at)
            .finish_non_exhaustive()
    }
}
impl PartialEq for VirtualFileDescriptor {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.is_directory == other.is_directory
            && self.size == other.size
            && self.modified_at == other.modified_at
            && Arc::ptr_eq(&self.provider, &other.provider)
    }
}
impl Eq for VirtualFileDescriptor {}

/// Regular virtual files in content-index order. Native destinations handle conflicts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualFileDragPayload(SmallVec<[VirtualFileDescriptor; 2]>, DragObserver);
impl VirtualFileDragPayload {
    /// Attach a gesture observer without resolving or opening any sources.
    pub fn with_observer(mut self, observer: Arc<dyn NativeFileDragObserver>) -> Self {
        self.1 = DragObserver(Some(observer));
        self
    }
    /// The observer associated with this gesture.
    pub fn observer(&self) -> Option<&Arc<dyn NativeFileDragObserver>> {
        self.1.0.as_ref()
    }
    /// Validate every filename before advertising the payload.
    pub fn new(files: impl IntoIterator<Item = VirtualFileDescriptor>) -> io::Result<Self> {
        Self::validate(files, false)
    }
    /// Validate a parent-before-child descriptor tree, including empty directories.
    pub fn new_tree(files: impl IntoIterator<Item = VirtualFileDescriptor>) -> io::Result<Self> {
        Self::validate(files, true)
    }
    fn validate(
        files: impl IntoIterator<Item = VirtualFileDescriptor>,
        tree: bool,
    ) -> io::Result<Self> {
        let files: SmallVec<[VirtualFileDescriptor; 2]> = files.into_iter().collect();
        if files.is_empty() || files.len() > 65536 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid virtual file count",
            ));
        }
        let mut names = std::collections::HashSet::new();
        let mut directories = std::collections::HashSet::new();
        for file in &files {
            let name = file.name.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "filename is not Unicode")
            })?;
            if tree {
                if let Some((parent, _)) = name.rsplit_once('\\') {
                    if !directories.contains(&parent.to_lowercase()) {
                        return Err(io::ErrorKind::InvalidInput.into());
                    }
                }
            }
            let full_name = name;
            if full_name.contains('\0') {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            for name in name.split(if tree { '\\' } else { '\0' }) {
                let stem = name
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_uppercase();
                let reserved_port = stem
                    .strip_prefix("COM")
                    .or_else(|| stem.strip_prefix("LPT"))
                    .is_some_and(|suffix| {
                        matches!(
                            suffix,
                            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                        )
                    });
                if name.is_empty()
                    || name == "."
                    || name == ".."
                    || name.ends_with(['.', ' '])
                    || name
                        .chars()
                        .any(|c| c.is_control() || "/\\:<>\"|?*".contains(c))
                    || full_name.encode_utf16().count() >= 260
                    || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    || reserved_port
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "unsafe or duplicate virtual filename",
                    ));
                }
            }
            if !names.insert(full_name.to_lowercase()) {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            if file.is_directory {
                directories.insert(full_name.to_lowercase());
            }
        }
        Ok(Self(files, DragObserver::default()))
    }
    /// Descriptors in native content-index order.
    pub fn files(&self) -> &[VirtualFileDescriptor] {
        &self.0
    }
    /// Cancel all sources without starting new source work.
    pub fn cancel(&self) {
        for file in &self.0 {
            file.provider.cancel();
        }
    }
}

/// Enumerates a virtual tree on the native worker. No file contents are read.
pub trait VirtualFileTreeProvider: Send + Sync + 'static {
    /// Load and validate the entire immutable descriptor tree on a worker.
    fn load(&self) -> io::Result<VirtualFileDragPayload>;
    /// Cancel enumeration without blocking the caller.
    fn cancel(&self);
}
struct DeferredTree {
    source: Arc<dyn VirtualFileTreeProvider>,
    result: OnceLock<io::Result<VirtualFileDragPayload>>,
    cancelled: AtomicBool,
}
/// Lazily frozen native descriptor tree. Resolution errors are cached as well.
#[derive(Clone)]
pub struct DeferredVirtualFileDragPayload(Arc<DeferredTree>, DragObserver);
impl fmt::Debug for DeferredVirtualFileDragPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeferredVirtualFileDragPayload")
            .finish_non_exhaustive()
    }
}
impl PartialEq for DeferredVirtualFileDragPayload {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) && self.1 == other.1
    }
}
impl Eq for DeferredVirtualFileDragPayload {}
impl DeferredVirtualFileDragPayload {
    /// Attach a gesture observer without resolving or opening any sources.
    pub fn with_observer(mut self, observer: Arc<dyn NativeFileDragObserver>) -> Self {
        self.1 = DragObserver(Some(observer));
        self
    }
    /// The observer associated with this gesture.
    pub fn observer(&self) -> Option<&Arc<dyn NativeFileDragObserver>> {
        self.1.0.as_ref()
    }
    /// Wrap a source without enumerating it or opening any content.
    pub fn new(source: Arc<dyn VirtualFileTreeProvider>) -> Self {
        Self(
            Arc::new(DeferredTree {
                source,
                result: OnceLock::new(),
                cancelled: AtomicBool::new(false),
            }),
            DragObserver::default(),
        )
    }
    /// May block; native workers only. The validated result is frozen for all indexes.
    pub fn resolve(&self) -> io::Result<&VirtualFileDragPayload> {
        if self.0.cancelled.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let result = self.0.result.get_or_init(|| self.0.source.load());
        if self.0.cancelled.load(Ordering::Acquire) {
            self.cancel();
            return Err(io::ErrorKind::Interrupted.into());
        }
        match result {
            Ok(files) => Ok(files),
            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
        }
    }
    /// Cancel enumeration and every stream created from the resolved tree.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.source.cancel();
        if let Some(Ok(files)) = self.0.result.get() {
            files.cancel();
        }
    }
}
impl Drop for DeferredTree {
    fn drop(&mut self) {
        self.source.cancel();
        if let Some(Ok(files)) = self.result.get() {
            files.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        VirtualFileDescriptor, VirtualFileDragPayload, VirtualFileProvider, VirtualFileStream,
    };
    use std::{
        io,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    struct Source(AtomicUsize);
    impl VirtualFileProvider for Source {
        fn open(&self) -> io::Result<Box<dyn VirtualFileStream>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(io::ErrorKind::Unsupported.into())
        }
        fn cancel(&self) {}
    }
    fn descriptor(name: &str, source: Arc<Source>) -> VirtualFileDescriptor {
        VirtualFileDescriptor {
            name: name.into(),
            is_directory: false,
            size: Some(0),
            modified_at: None,
            provider: source,
        }
    }
    #[test]
    fn tree_preserves_empty_directories_and_rejects_missing_or_unsafe_parents() {
        let source = Arc::new(Source(AtomicUsize::new(0)));
        let mut directory = descriptor("folder", source.clone());
        directory.is_directory = true;
        let mut empty = descriptor("folder\\empty", source.clone());
        empty.is_directory = true;
        let payload = VirtualFileDragPayload::new_tree([
            directory.clone(),
            empty,
            descriptor("folder\\data.txt", source.clone()),
        ])
        .unwrap();
        assert_eq!(payload.files().len(), 3);
        assert!(payload.files()[1].is_directory);
        assert_eq!(source.0.load(Ordering::SeqCst), 0);
        for name in [
            "folder\\..\\escape",
            "folder\\NUL.txt",
            "folder\\name:stream",
            "folder\\a\0b",
            "folder\\",
            "missing\\file",
        ] {
            assert!(
                VirtualFileDragPayload::new_tree([
                    directory.clone(),
                    descriptor(name, source.clone())
                ])
                .is_err(),
                "{name:?}"
            );
        }
        assert!(
            VirtualFileDragPayload::new_tree([
                descriptor("folder", source.clone()),
                descriptor("folder\\file", source)
            ])
            .is_err()
        );
    }
    struct TreeSource(Arc<Source>, AtomicUsize);
    impl super::VirtualFileTreeProvider for TreeSource {
        fn load(&self) -> io::Result<VirtualFileDragPayload> {
            self.1.fetch_add(1, Ordering::SeqCst);
            VirtualFileDragPayload::new([descriptor("file", self.0.clone())])
        }
        fn cancel(&self) {}
    }
    #[test]
    fn deferred_tree_is_frozen_once_and_cancel_before_resolve_never_enumerates() {
        let source = Arc::new(TreeSource(
            Arc::new(Source(AtomicUsize::new(0))),
            AtomicUsize::new(0),
        ));
        let payload = super::DeferredVirtualFileDragPayload::new(source.clone());
        assert_eq!(source.1.load(Ordering::SeqCst), 0);
        assert_eq!(payload.resolve().unwrap().files().len(), 1);
        assert_eq!(payload.resolve().unwrap().files().len(), 1);
        assert_eq!(source.1.load(Ordering::SeqCst), 1);
        assert_eq!(source.0.0.load(Ordering::SeqCst), 0);
        let cancelled = super::DeferredVirtualFileDragPayload::new(source.clone());
        cancelled.cancel();
        assert!(cancelled.resolve().is_err());
        assert_eq!(source.1.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn metadata_and_cancel_never_open_content() {
        let source = Arc::new(Source(AtomicUsize::new(0)));
        let payload =
            VirtualFileDragPayload::new([descriptor("你好.txt", source.clone())]).unwrap();
        assert_eq!(payload.files()[0].size, Some(0));
        payload.cancel();
        assert_eq!(source.0.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn rejects_paths_devices_and_duplicate_names() {
        let source = Arc::new(Source(AtomicUsize::new(0)));
        for name in [
            "",
            ".",
            "..",
            "../file",
            "a\\b",
            "x:y",
            "NUL.txt",
            "COM1",
            "COM¹.txt",
            "LPT²",
            "a.",
            "a\0b",
        ] {
            assert!(VirtualFileDragPayload::new([descriptor(name, source.clone())]).is_err());
        }
        assert!(
            VirtualFileDragPayload::new([
                descriptor("A.txt", source.clone()),
                descriptor("a.txt", source)
            ])
            .is_err()
        );
    }
}
