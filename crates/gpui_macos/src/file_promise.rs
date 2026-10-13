use block2::DynBlock;
use gpui::PromisedFileDescriptor;
use objc2::{
    AnyThread, DefinedClass, define_class, msg_send, rc::Retained, runtime::ProtocolObject,
};
use objc2_app_kit::{NSFilePromiseProvider, NSFilePromiseProviderDelegate};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSOperationQueue, NSString, NSURL};
use std::{
    ffi::{CStr, OsStr},
    os::unix::ffi::OsStrExt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::OnceLock,
};

struct PromiseIvars {
    file: PromisedFileDescriptor,
    queue: Retained<NSOperationQueue>,
}
impl Drop for PromiseIvars {
    fn drop(&mut self) {
        self.file.provider.cancel();
    }
}
define_class!(
    // SAFETY: NSObject has no subclassing requirements; ivars own their Rust values.
    #[unsafe(super(NSObject))]
    #[ivars = PromiseIvars]
    struct FilePromiseDelegate;

    unsafe impl NSObjectProtocol for FilePromiseDelegate {}
    unsafe impl NSFilePromiseProviderDelegate for FilePromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(
            &self,
            _provider: &NSFilePromiseProvider,
            _file_type: &NSString,
        ) -> Retained<NSString> {
            NSString::from_str(&self.ivars().file.name.to_string_lossy())
        }
        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn operation_queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_promise(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            completion: &DynBlock<dyn Fn(*mut NSError)>,
        ) {
            // AppKit calls this on the supplied operation queue. Its URL is the
            // complete destination, including the filename negotiated with Finder.
            let result = catch_unwind(AssertUnwindSafe(|| {
                if !url.isFileURL() {
                    return Err(std::io::ErrorKind::InvalidInput.into());
                }
                let bytes = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) };
                self.ivars()
                    .file
                    .provider
                    .write_to(Path::new(OsStr::from_bytes(bytes.to_bytes())))
            }));
            if matches!(result, Ok(Ok(()))) {
                completion.call((std::ptr::null_mut(),));
            } else {
                // Do not expose remote paths, terminal data, or credentials in diagnostics.
                let error = unsafe {
                    NSError::errorWithDomain_code_userInfo(
                        &NSString::from_str("GPUIFilePromise"),
                        1,
                        None,
                    )
                };
                completion.call((Retained::as_ptr(&error).cast_mut(),));
            }
        }
    }
);

pub(crate) fn provider(file: PromisedFileDescriptor) -> Retained<NSFilePromiseProvider> {
    let file_type = NSString::from_str(if file.is_directory {
        "public.folder"
    } else {
        "public.data"
    });
    // One queue for the entire application bounds simultaneous blocking writes,
    // even when several windows each advertise many promises.
    static QUEUE: OnceLock<Retained<NSOperationQueue>> = OnceLock::new();
    let queue = QUEUE
        .get_or_init(|| {
            let queue = NSOperationQueue::new();
            queue.setMaxConcurrentOperationCount(3);
            queue
        })
        .clone();
    let delegate = FilePromiseDelegate::alloc().set_ivars(PromiseIvars { file, queue });
    let delegate: Retained<FilePromiseDelegate> = unsafe { msg_send![super(delegate), init] };
    let provider = NSFilePromiseProvider::initWithFileType_delegate(
        NSFilePromiseProvider::alloc(),
        &file_type,
        ProtocolObject::from_ref(&*delegate),
    );
    // The delegate property is weak. userInfo retains it for precisely the
    // promise provider's lifetime, without retaining an application entity.
    unsafe {
        provider.setUserInfo(Some(&delegate));
    }
    provider
}

#[cfg(test)]
mod tests {
    use super::provider;
    use block2::RcBlock;
    use gpui::{PromisedFileDescriptor, PromisedFileProvider};
    use objc2::{
        msg_send,
        rc::{Retained, autoreleasepool},
    };
    use objc2_app_kit::NSFilePromiseProvider;
    use objc2_foundation::{NSError, NSOperationQueue, NSString, NSURL};
    use std::{
        io,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    struct Source {
        received: Mutex<Vec<PathBuf>>,
        cancelled: AtomicBool,
        fail: bool,
    }
    impl PromisedFileProvider for Source {
        fn write_to(&self, target: &Path) -> io::Result<()> {
            self.received
                .lock()
                .expect("received paths")
                .push(target.to_path_buf());
            if self.fail {
                Err(io::Error::other("injected failure"))
            } else {
                Ok(())
            }
        }
        fn cancel(&self) {
            self.cancelled.store(true, Ordering::Release);
        }
    }
    #[test]
    fn native_promise_delegate_retains_source_and_delivers_exact_destination_once() {
        for directory in [false, true] {
            for fail in [false, true] {
                let source = Arc::new(Source {
                    received: Mutex::new(Vec::new()),
                    cancelled: AtomicBool::new(false),
                    fail,
                });
                autoreleasepool(|_| {
                    let promise = provider(PromisedFileDescriptor {
                        name: "你好".into(),
                        is_directory: directory,
                        provider: source.clone(),
                    });
                    let delegate = promise.delegate().expect("retained promise delegate");
                    // These Objective-C dispatches exercise the runtime ABI, including
                    // autoreleased object returns; no Finder or file writes are needed.
                    let file_type = NSString::from_str(if directory {
                        "public.folder"
                    } else {
                        "public.data"
                    });
                    let name: Retained<NSString> = unsafe {
                        msg_send![&*delegate, filePromiseProvider: &*promise fileNameForType: &*file_type]
                    };
                    assert_eq!(name.to_string(), "你好");
                    let queue: Retained<NSOperationQueue> = unsafe {
                        msg_send![&*delegate, operationQueueForFilePromiseProvider: &*promise]
                    };
                    assert_eq!(queue.maxConcurrentOperationCount(), 3);
                    assert!(!source.cancelled.load(Ordering::Acquire));
                    let url = NSURL::fileURLWithPath(&NSString::from_str("/tmp/GPUI promise/你好"));
                    let calls = AtomicUsize::new(0);
                    let completion = RcBlock::new(|error: *mut NSError| {
                        assert_eq!(!error.is_null(), fail);
                        calls.fetch_add(1, Ordering::SeqCst);
                    });
                    let _: () = unsafe {
                        msg_send![&*delegate, filePromiseProvider: &*promise writePromiseToURL: &*url completionHandler: &*completion]
                    };
                    assert_eq!(calls.load(Ordering::SeqCst), 1);
                    assert_eq!(
                        *source.received.lock().expect("received paths"),
                        [PathBuf::from("/tmp/GPUI promise/你好")]
                    );
                    let _: &NSFilePromiseProvider = &promise;
                });
                assert!(source.cancelled.load(Ordering::Acquire));
            }
        }
    }
}
