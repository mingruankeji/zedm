use crate::external_drag::{
    ContentStream, DragDataObject, DragExportSession, DragInputGuard, format,
};
use gpui::{
    ExternalDragPayload, FileDragPaths, VIRTUAL_FILE_CHUNK_SIZE, VirtualFileDescriptor,
    VirtualFileDragPayload, VirtualFileProvider, VirtualFileStream,
};
use std::{
    ffi::c_void,
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};
use windows::{
    Win32::{
        Foundation::{DV_E_LINDEX, DV_E_TYMED, S_FALSE, S_OK, STG_E_READFAULT, STG_E_REVERTED},
        System::{
            Com::{
                IDataObject, IStream, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
                TYMED_HGLOBAL, TYMED_ISTREAM,
            },
            DataExchange::RegisterClipboardFormatW,
            Ole::{CF_HDROP, ReleaseStgMedium},
        },
        UI::Shell::{DragQueryFileW, HDROP, IDataObjectAsyncCapability},
    },
    core::{HRESULT, Interface, w},
};

struct MockSource {
    content: Arc<Vec<u8>>,
    opens: Arc<AtomicUsize>,
    cancelled: Arc<AtomicBool>,
    fail: bool,
}
struct MockStream {
    content: Arc<Vec<u8>>,
    cancelled: Arc<AtomicBool>,
    fail: bool,
}
impl VirtualFileProvider for MockSource {
    fn open(&self) -> io::Result<Box<dyn VirtualFileStream>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockStream {
            content: self.content.clone(),
            cancelled: self.cancelled.clone(),
            fail: self.fail,
        }))
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}
impl VirtualFileStream for MockStream {
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        assert!(buffer.len() <= VIRTUAL_FILE_CHUNK_SIZE);
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.fail {
            return Err(io::Error::other("mock failure"));
        }
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        if offset >= self.content.len() {
            return Ok(0);
        }
        let count = buffer.len().min(self.content.len() - offset).min(317);
        buffer[..count].copy_from_slice(&self.content[offset..offset + count]);
        Ok(count)
    }
    fn cancel(&self) {}
}
fn source(bytes: Vec<u8>, fail: bool) -> Arc<MockSource> {
    Arc::new(MockSource {
        content: Arc::new(bytes),
        opens: Arc::new(AtomicUsize::new(0)),
        cancelled: Arc::new(AtomicBool::new(false)),
        fail,
    })
}
fn descriptor(name: &str, source: Arc<MockSource>) -> VirtualFileDescriptor {
    VirtualFileDescriptor {
        is_directory: false,
        name: name.into(),
        size: Some(source.content.len() as u64),
        modified_at: None,
        provider: source,
    }
}
fn session(payload: ExternalDragPayload, lifetime: &Arc<()>) -> Arc<DragExportSession> {
    Arc::new(DragExportSession {
        payload,
        resolved_payload: std::sync::OnceLock::new(),
        cancelled: AtomicBool::new(false),
        content_allowed: AtomicBool::new(true),
        window_lifetime: Arc::downgrade(lifetime),
        started: Instant::now(),
        activity_ms: AtomicU64::new(0),
        source_validation: 7,
        finished: AtomicBool::new(false),
        dropped: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        active: AtomicUsize::new(0),
    })
}
fn data(session: Arc<DragExportSession>) -> IDataObject {
    DragDataObject {
        session,
        async_mode: std::cell::Cell::new(true),
        in_operation: std::cell::Cell::new(false),
        descriptor_format: unsafe { RegisterClipboardFormatW(w!("FileGroupDescriptorW")) } as u16,
        content_format: unsafe { RegisterClipboardFormatW(w!("FileContents")) } as u16,
    }
    .into()
}
fn content(data: &IDataObject, index: i32) -> IStream {
    let request = format(
        unsafe { RegisterClipboardFormatW(w!("FileContents")) } as u16,
        TYMED_ISTREAM,
        index,
    );
    let mut medium = unsafe { data.GetData(&request).unwrap() };
    let stream = unsafe { medium.u.pstm.as_ref().unwrap().clone() };
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    stream
}
fn read(stream: &IStream, length: usize) -> (HRESULT, Vec<u8>) {
    let mut bytes = vec![0; length];
    let mut count = 0;
    let result = unsafe {
        stream.Read(
            bytes.as_mut_ptr() as *mut c_void,
            length as u32,
            Some(&mut count),
        )
    };
    bytes.truncate(count as usize);
    (result, bytes)
}

#[test]
fn virtual_data_object_defers_open_and_reads_multiple_unicode_files() {
    let lifetime = Arc::new(());
    let first = source((0..200_000).map(|i| (i % 251) as u8).collect(), false);
    let second = source(Vec::new(), false);
    let payload = VirtualFileDragPayload::new([
        descriptor("你好.bin", first.clone()),
        descriptor("empty", second.clone()),
    ])
    .unwrap();
    let session = session(ExternalDragPayload::VirtualFiles(payload), &lifetime);
    let data = data(session.clone());
    let request = format(
        unsafe { RegisterClipboardFormatW(w!("FileGroupDescriptorW")) } as u16,
        TYMED_HGLOBAL,
        -1,
    );
    let mut medium = unsafe { data.GetData(&request).unwrap() };
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    let stream = content(&data, 0);
    assert_eq!(first.opens.load(Ordering::SeqCst), 0);
    let (result, bytes) = read(&stream, 200_001);
    assert_eq!(result, S_FALSE);
    assert_eq!(&bytes, first.content.as_ref());
    assert_eq!(read(&stream, 1), (S_FALSE, Vec::new()));
    assert_eq!(read(&content(&data, 1), 1), (S_FALSE, Vec::new()));
    drop(data);
    assert!(!first.cancelled.load(Ordering::SeqCst));
    drop(stream);
    assert_eq!(Arc::strong_count(&session), 1);
    drop(session);
    assert!(first.cancelled.load(Ordering::SeqCst));
}

#[test]
fn streams_support_seek_clone_and_independent_reopen() {
    let lifetime = Arc::new(());
    let source = source(b"0123456789".to_vec(), false);
    let payload = VirtualFileDragPayload::new([descriptor("test", source.clone())]).unwrap();
    let data = data(session(
        ExternalDragPayload::VirtualFiles(payload),
        &lifetime,
    ));
    let stream = content(&data, 0);
    unsafe {
        stream.Seek(7, STREAM_SEEK_SET, None).unwrap();
    }
    assert_eq!(read(&stream, 2).1, b"78");
    let clone = unsafe { stream.Clone().unwrap() };
    assert_eq!(read(&clone, 1).1, b"9");
    unsafe {
        stream.Seek(-4, STREAM_SEEK_CUR, None).unwrap();
    }
    assert_eq!(read(&stream, 2).1, b"56");
    unsafe {
        stream.Seek(-2, STREAM_SEEK_END, None).unwrap();
    }
    assert_eq!(read(&stream, 2).1, b"89");
    assert!(unsafe { stream.Seek(-1, STREAM_SEEK_SET, None) }.is_err());
    assert_eq!(read(&content(&data, 0), 2).1, b"01");
}

#[test]
fn cancelled_and_failed_sources_never_return_false_success() {
    let lifetime = Arc::new(());
    let source = source(b"data".to_vec(), true);
    let payload = VirtualFileDragPayload::new([descriptor("test", source.clone())]).unwrap();
    let session = session(ExternalDragPayload::VirtualFiles(payload), &lifetime);
    let data = data(session.clone());
    let stream = content(&data, 0);
    assert_eq!(read(&stream, 4).0, STG_E_READFAULT);
    session.cancel();
    assert_eq!(read(&stream, 4).0, STG_E_REVERTED);
    assert_eq!(source.opens.load(Ordering::SeqCst), 1);
}

#[test]
fn metadata_only_cancel_does_not_open_and_window_drop_cancels() {
    let lifetime = Arc::new(());
    let source = source(b"data".to_vec(), false);
    let payload = VirtualFileDragPayload::new([descriptor("test", source.clone())]).unwrap();
    let session = session(ExternalDragPayload::VirtualFiles(payload), &lifetime);
    let data = data(session.clone());
    let stream = content(&data, 0);
    drop(lifetime);
    assert_eq!(read(&stream, 1).0, STG_E_REVERTED);
    assert_eq!(source.opens.load(Ordering::SeqCst), 0);
    assert!(source.cancelled.load(Ordering::SeqCst));
}

#[test]
fn local_files_offer_hdrop_with_unicode_and_strict_format_validation() {
    let lifetime = Arc::new(());
    let data = data(session(
        ExternalDragPayload::Files(FileDragPaths::new([
            (PathBuf::from(r"C:\你好.txt"), false),
            (PathBuf::from(r"C:\folder"), true),
        ])),
        &lifetime,
    ));
    let request = format(CF_HDROP.0, TYMED_HGLOBAL, -1);
    assert_eq!(unsafe { data.QueryGetData(&request) }, S_OK);
    let mut medium = unsafe { data.GetData(&request).unwrap() };
    assert_eq!(
        unsafe { DragQueryFileW(HDROP(medium.u.hGlobal.0), u32::MAX, None) },
        2
    );
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    assert_eq!(
        unsafe { data.QueryGetData(&format(CF_HDROP.0, TYMED_ISTREAM, -1)) },
        DV_E_TYMED
    );
    assert_eq!(
        unsafe { data.QueryGetData(&format(CF_HDROP.0, TYMED_HGLOBAL, 0)) },
        DV_E_LINDEX
    );
    let capability: IDataObjectAsyncCapability = data.cast().unwrap();
    assert!(unsafe { capability.GetAsyncMode().unwrap() }.as_bool());
}

#[test]
fn advertised_size_requires_real_eof_or_returns_failure() {
    let lifetime = Arc::new(());
    let source = source(b"short".to_vec(), false);
    let mut descriptor = descriptor("file", source);
    descriptor.size = Some(100);
    let payload = VirtualFileDragPayload::new([descriptor.clone()]).unwrap();
    let stream: IStream = ContentStream::new(
        descriptor,
        session(ExternalDragPayload::VirtualFiles(payload), &lifetime),
    )
    .into();
    assert_eq!(read(&stream, 100).0, STG_E_READFAULT);
}

#[test]
fn hover_content_probe_returns_pending_without_opening() {
    let lifetime = Arc::new(());
    let source = source(b"data".to_vec(), false);
    let payload = VirtualFileDragPayload::new([descriptor("test", source.clone())]).unwrap();
    let session = session(ExternalDragPayload::VirtualFiles(payload), &lifetime);
    session.content_allowed.store(false, Ordering::Release);
    let data = data(session.clone());
    let stream = content(&data, 0);
    assert_eq!(read(&stream, 1).0, HRESULT(0x8000000A_u32 as i32));
    assert_eq!(source.opens.load(Ordering::SeqCst), 0);
    session.content_allowed.store(true, Ordering::Release);
    assert_eq!(read(&stream, 1).1, b"d");
}

#[test]
fn empty_file_requests_source_eof_and_releases_session() {
    let lifetime = Arc::new(());
    let source = source(Vec::new(), false);
    let session = session(
        ExternalDragPayload::VirtualFiles(
            VirtualFileDragPayload::new([descriptor("empty", source.clone())]).unwrap(),
        ),
        &lifetime,
    );
    let weak = Arc::downgrade(&session);
    let data = data(session);
    let stream = content(&data, 0);
    let (result, bytes) = read(&stream, 10);
    assert_eq!(result, S_FALSE);
    assert!(bytes.is_empty());
    assert_eq!(source.opens.load(Ordering::SeqCst), 1);
    drop(stream);
    drop(data);
    assert!(weak.upgrade().is_none());
}

#[test]
fn source_marker_restores_only_its_original_window() {
    let lifetime = Arc::new(());
    let source = source(b"data".to_vec(), false);
    let payload = VirtualFileDragPayload::new([descriptor("test", source.clone())]).unwrap();
    let data = data(session(
        ExternalDragPayload::VirtualFiles(payload),
        &lifetime,
    ));
    assert!(crate::external_drag::is_source_drag(&data, 7));
    assert!(!crate::external_drag::is_source_drag(&data, 8));
    assert_eq!(source.opens.load(Ordering::SeqCst), 0);
}

#[test]
fn native_worker_handoff_preserves_initiating_button_and_modifier_state() {
    use windows::Win32::{
        System::Threading::GetCurrentThreadId,
        UI::{
            Input::KeyboardAndMouse::{GetKeyboardState, VK_LBUTTON, VK_SHIFT},
            WindowsAndMessaging::{MSG, PM_NOREMOVE, PeekMessageW},
        },
    };
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let source = std::thread::spawn(move || {
        unsafe {
            let mut message = MSG::default();
            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            ready_tx.send(GetCurrentThreadId()).unwrap();
        }
        stop_rx.recv().unwrap();
    });
    let source_thread = ready_rx.recv().unwrap();
    let worker = std::thread::spawn(move || {
        let mut keyboard = [0_u8; 256];
        keyboard[VK_LBUTTON.0 as usize] = 0x80;
        keyboard[VK_SHIFT.0 as usize] = 0x80;
        let input = DragInputGuard::attach(source_thread, &keyboard).unwrap();
        let mut actual = [0_u8; 256];
        unsafe { GetKeyboardState(&mut actual) }.unwrap();
        assert_eq!(actual[VK_LBUTTON.0 as usize] & 0x80, 0x80);
        assert_eq!(actual[VK_SHIFT.0 as usize] & 0x80, 0x80);
        drop(input);
        // Reattachment also succeeds after cleanup; the guard leaves no association.
        drop(DragInputGuard::attach(source_thread, &[0_u8; 256]).unwrap());
    });
    let result = worker.join();
    stop_tx.send(()).unwrap();
    source.join().unwrap();
    result.unwrap();
}

#[test]
fn deferred_directory_descriptors_preserve_empty_folders_and_content_indexes() {
    struct Tree {
        file: Arc<MockSource>,
        loads: AtomicUsize,
    }
    impl gpui::VirtualFileTreeProvider for Tree {
        fn load(&self) -> io::Result<VirtualFileDragPayload> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            let mut root = descriptor("folder", self.file.clone());
            root.is_directory = true;
            root.size = None;
            let mut empty = descriptor("folder\\empty", self.file.clone());
            empty.is_directory = true;
            empty.size = None;
            VirtualFileDragPayload::new_tree([
                root,
                empty,
                descriptor("folder\\data.txt", self.file.clone()),
            ])
        }
        fn cancel(&self) {
            self.file.cancel();
        }
    }
    let lifetime = Arc::new(());
    let tree = Arc::new(Tree {
        file: source(b"nested".to_vec(), false),
        loads: AtomicUsize::new(0),
    });
    let data = data(session(
        ExternalDragPayload::VirtualFileTree(gpui::DeferredVirtualFileDragPayload::new(
            tree.clone(),
        )),
        &lifetime,
    ));
    assert_eq!(tree.loads.load(Ordering::SeqCst), 0);
    let descriptor_format = unsafe { RegisterClipboardFormatW(w!("FileGroupDescriptorW")) } as u16;
    let content_format = unsafe { RegisterClipboardFormatW(w!("FileContents")) } as u16;
    let mut medium = unsafe {
        data.GetData(&format(descriptor_format, TYMED_HGLOBAL, -1))
            .unwrap()
    };
    unsafe {
        use windows::Win32::{
            Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY,
            System::Memory::{GlobalLock, GlobalUnlock},
            UI::Shell::FILEDESCRIPTORW,
        };
        let pointer = GlobalLock(medium.u.hGlobal) as *const u8;
        assert_eq!(std::ptr::read_unaligned(pointer.cast::<u32>()), 3);
        let root = std::ptr::read_unaligned(pointer.add(4).cast::<FILEDESCRIPTORW>());
        let empty = std::ptr::read_unaligned(
            pointer
                .add(4 + std::mem::size_of::<FILEDESCRIPTORW>())
                .cast::<FILEDESCRIPTORW>(),
        );
        assert_ne!(root.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0, 0);
        assert_ne!(empty.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0, 0);
        GlobalUnlock(medium.u.hGlobal).ok();
        ReleaseStgMedium(&mut medium);
        assert_eq!(
            data.QueryGetData(&format(content_format, TYMED_ISTREAM, 0)),
            DV_E_LINDEX
        );
        assert_eq!(
            data.QueryGetData(&format(content_format, TYMED_ISTREAM, 1)),
            DV_E_LINDEX
        );
    }
    assert_eq!(tree.loads.load(Ordering::SeqCst), 1);
    assert_eq!(tree.file.opens.load(Ordering::SeqCst), 0);
    assert_eq!(read(&content(&data, 2), 100).1, b"nested");
    assert_eq!(tree.loads.load(Ordering::SeqCst), 1);
}

#[test]
fn native_completion_is_reported_once_even_when_cancel_and_release_follow() {
    struct Observer(std::sync::Mutex<Vec<gpui::NativeFileDragEvent>>);
    impl gpui::NativeFileDragObserver for Observer {
        fn observe(&self, event: gpui::NativeFileDragEvent) {
            self.0.lock().unwrap().push(event);
        }
    }
    let observer = Arc::new(Observer(std::sync::Mutex::new(Vec::new())));
    let lifetime = Arc::new(());
    let payload =
        VirtualFileDragPayload::new([descriptor("file", source(b"file".to_vec(), false))])
            .unwrap()
            .with_observer(observer.clone());
    let session = session(ExternalDragPayload::VirtualFiles(payload), &lifetime);
    let data = data(session.clone());
    let capability: IDataObjectAsyncCapability = data.cast().unwrap();
    unsafe {
        capability.StartOperation(None).unwrap();
        capability.EndOperation(S_OK, None, 0).unwrap();
        capability.EndOperation(STG_E_READFAULT, None, 0).unwrap();
    }
    drop(capability);
    drop(data);
    session.cancel();
    drop(session);
    assert_eq!(
        *observer.0.lock().unwrap(),
        [gpui::NativeFileDragEvent::Finished(
            gpui::NativeFileDragOutcome::Provided
        )]
    );
}

#[test]
fn native_idle_expiry_excludes_active_operations_and_user_pauses() {
    struct Paused;
    impl gpui::NativeFileDragObserver for Paused {
        fn observe(&self, _: gpui::NativeFileDragEvent) {}
        fn is_paused(&self) -> bool {
            true
        }
    }
    let lifetime = Arc::new(());
    let mut session = session(
        ExternalDragPayload::VirtualFiles(
            VirtualFileDragPayload::new([descriptor("file", source(vec![], false))]).unwrap(),
        ),
        &lifetime,
    );
    Arc::get_mut(&mut session).unwrap().started =
        Instant::now() - std::time::Duration::from_secs(600);
    assert!(session.idle_expired());
    {
        let _operation = session.operation();
        assert!(!session.idle_expired());
    }
    assert!(!session.idle_expired());
    Arc::get_mut(&mut session)
        .unwrap()
        .activity_ms
        .store(0, Ordering::Release);
    let payload = VirtualFileDragPayload::new([descriptor("file", source(vec![], false))])
        .unwrap()
        .with_observer(Arc::new(Paused));
    Arc::get_mut(&mut session).unwrap().payload = ExternalDragPayload::VirtualFiles(payload);
    assert!(!session.idle_expired());
}
