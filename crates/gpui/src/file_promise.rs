use std::{ffi::OsString, fmt, io, path::Path, sync::Arc};

/// Writes a promised file or directory only after a receiver supplies its path.
pub trait PromisedFileProvider: Send + Sync + 'static {
    /// Called on a worker after a drop; target is the full final path.
    fn write_to(&self, target: &Path) -> io::Result<()>;
    /// Cancel writes without blocking the caller.
    fn cancel(&self);
}

/// One top-level item advertised to the native promise receiver.
#[derive(Clone)]
pub struct PromisedFileDescriptor {
    /// A single safe display name.
    pub name: OsString,
    /// Whether the promise creates a directory.
    pub is_directory: bool,
    /// Worker-only implementation of the promise.
    pub provider: Arc<dyn PromisedFileProvider>,
}

/// Validated ordered top-level promises.
#[derive(Clone)]
pub struct PromisedFileDragPayload(Vec<PromisedFileDescriptor>);
impl fmt::Debug for PromisedFileDragPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromisedFileDragPayload")
            .field("count", &self.0.len())
            .finish()
    }
}
impl PartialEq for PromisedFileDragPayload {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self.0.iter().zip(&other.0).all(|(a, b)| {
                a.name == b.name
                    && a.is_directory == b.is_directory
                    && Arc::ptr_eq(&a.provider, &b.provider)
            })
    }
}
impl Eq for PromisedFileDragPayload {}
impl PromisedFileDragPayload {
    /// Validate every item before it is advertised.
    pub fn new(files: Vec<PromisedFileDescriptor>) -> io::Result<Self> {
        if files.is_empty() || files.len() > 1024 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut names = std::collections::HashSet::new();
        for file in &files {
            let name = file.name.to_str().ok_or(io::ErrorKind::InvalidInput)?;
            if name.is_empty()
                || matches!(name, "." | "..")
                || name.chars().any(|c| c.is_control() || "/:\\".contains(c))
                || !names.insert(name.to_lowercase())
            {
                return Err(io::ErrorKind::InvalidInput.into());
            }
        }
        Ok(Self(files))
    }
    /// Top-level items in native drag order.
    pub fn files(&self) -> &[PromisedFileDescriptor] {
        &self.0
    }
    /// Cancel every pending write.
    pub fn cancel(&self) {
        for file in &self.0 {
            file.provider.cancel();
        }
    }
}
