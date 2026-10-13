//! Shell file drag sources. OLE runs in its own STA: a synchronous consumer can
//! wait for bounded provider requests without ever waiting on GPUI's UI thread.

use gpui::{
    ExternalDragPayload, NativeFileDragEvent, NativeFileDragObserver, NativeFileDragOutcome,
    VIRTUAL_FILE_CHUNK_SIZE, VirtualFileDescriptor, VirtualFileStream,
};
use gpui_util::ResultExt as _;
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    mem::{ManuallyDrop, size_of},
    os::windows::ffi::OsStrExt,
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Instant, UNIX_EPOCH},
};
use windows::{
    Win32::{
        Foundation::{
            DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
            DV_E_DVASPECT, DV_E_DVTARGETDEVICE, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED,
            E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, FILETIME, GlobalFree, HGLOBAL, HWND,
            LPARAM, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK, STG_E_ACCESSDENIED,
            STG_E_INVALIDFUNCTION, STG_E_READFAULT, STG_E_REVERTED, STG_E_WRITEFAULT, WPARAM,
        },
        System::{
            Com::{
                DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IBindCtx, IDataObject,
                IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream,
                IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGM_READ, STGMEDIUM, STGMEDIUM_0,
                STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
                TYMED, TYMED_HGLOBAL, TYMED_ISTREAM,
            },
            DataExchange::RegisterClipboardFormatW,
            Memory::{
                GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
            },
            Ole::{
                CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource,
                IDropSource_Impl, OleInitialize, OleUninitialize, ReleaseStgMedium,
            },
            SystemServices::MK_LBUTTON,
            Threading::{AttachThreadInput, GetCurrentThreadId},
        },
        UI::{
            Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState},
            Shell::{
                DROPFILES, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FD_WRITESTIME,
                FILEDESCRIPTORW, IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl,
                SHCreateStdEnumFmtEtc,
            },
            WindowsAndMessaging::{
                DispatchMessageW, GetWindowThreadProcessId, MSG, MWMO_INPUTAVAILABLE,
                MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostMessageW, QS_ALLINPUT,
                TranslateMessage,
            },
        },
    },
    core::{BOOL, HRESULT, Ref, Result, implement, w},
};

pub(crate) struct DragExportSession {
    pub(crate) payload: ExternalDragPayload,
    pub(crate) resolved_payload: OnceLock<std::result::Result<ExternalDragPayload, HRESULT>>,
    pub(crate) cancelled: AtomicBool,
    pub(crate) content_allowed: AtomicBool,
    pub(crate) window_lifetime: Weak<()>,
    pub(crate) started: Instant,
    pub(crate) activity_ms: AtomicU64,
    pub(crate) source_validation: usize,
    pub(crate) finished: AtomicBool,
    pub(crate) dropped: AtomicBool,
    pub(crate) failed: AtomicBool,
    pub(crate) active: AtomicUsize,
}

impl DragExportSession {
    fn observer(&self) -> Option<&Arc<dyn NativeFileDragObserver>> {
        match &self.payload {
            ExternalDragPayload::VirtualFiles(files) => files.observer(),
            ExternalDragPayload::VirtualFileTree(tree) => tree.observer(),
            _ => None,
        }
    }
    fn observe(&self, event: NativeFileDragEvent) {
        if let Some(observer) = self.observer() {
            observer.observe(event);
        }
    }
    pub(crate) fn finish(&self, outcome: NativeFileDragOutcome) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            self.observe(NativeFileDragEvent::Finished(outcome));
        }
    }
    pub(crate) fn idle_expired(&self) -> bool {
        if self.active.load(Ordering::Acquire) > 0
            || self.observer().is_some_and(|observer| observer.is_paused())
        {
            self.touch();
            return false;
        }
        (self.started.elapsed().as_millis() as u64)
            .saturating_sub(self.activity_ms.load(Ordering::Relaxed))
            > 300_000
    }
    pub(crate) fn operation(&self) -> DragOperation<'_> {
        self.active.fetch_add(1, Ordering::AcqRel);
        self.touch();
        DragOperation(self)
    }
    pub(crate) fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            self.finish(NativeFileDragOutcome::Cancelled);
            match &self.payload {
                ExternalDragPayload::VirtualFiles(files) => files.cancel(),
                ExternalDragPayload::VirtualFileTree(tree) => tree.cancel(),
                _ => {}
            }
        }
    }
    fn payload(&self) -> Result<&ExternalDragPayload> {
        let _operation = self.operation();
        if let ExternalDragPayload::VirtualFileTree(tree) = &self.payload {
            match self.resolved_payload.get_or_init(|| {
                tree.resolve()
                    .map(|files| ExternalDragPayload::VirtualFiles(files.clone()))
                    .map_err(|_| STG_E_READFAULT)
            }) {
                Ok(payload) => Ok(payload),
                Err(error) => {
                    self.failed.store(true, Ordering::Release);
                    Err((*error).into())
                }
            }
        } else {
            Ok(&self.payload)
        }
    }
    fn touch(&self) {
        self.activity_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
    fn is_cancelled(&self) -> bool {
        if self.window_lifetime.upgrade().is_none() {
            self.cancel();
        }
        self.cancelled.load(Ordering::Acquire)
    }
}

pub(crate) struct DragOperation<'a>(&'a DragExportSession);
impl Drop for DragOperation<'_> {
    fn drop(&mut self) {
        self.0.touch();
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for DragExportSession {
    fn drop(&mut self) {
        self.finish(if self.failed.load(Ordering::Acquire) {
            NativeFileDragOutcome::Failed
        } else if self.dropped.load(Ordering::Acquire) {
            NativeFileDragOutcome::Provided
        } else {
            NativeFileDragOutcome::Cancelled
        });
        self.cancel();
    }
}

/// Starts OLE without retaining an entity or calling content providers on GPUI.
pub(crate) fn start(
    payload: ExternalDragPayload,
    window_lifetime: Weak<()>,
    source_hwnd: isize,
    source_validation: usize,
) -> bool {
    if matches!(&payload, ExternalDragPayload::PromisedFiles(_)) {
        return false;
    }
    if matches!(&payload, ExternalDragPayload::Files(paths) if paths.entries().is_empty()) {
        return false;
    }
    let mut keyboard = [0_u8; 256];
    if unsafe { GetKeyboardState(&mut keyboard) }
        .log_err()
        .is_none()
    {
        return false;
    }
    let source_thread = unsafe { GetWindowThreadProcessId(HWND(source_hwnd as *mut c_void), None) };
    if source_thread == 0 {
        return false;
    }
    std::thread::Builder::new()
        .name("gpui-file-drag".into())
        .spawn(move || {
            let session = Arc::new(DragExportSession {
                payload,
                resolved_payload: OnceLock::new(),
                cancelled: AtomicBool::new(false),
                content_allowed: AtomicBool::new(false),
                window_lifetime,
                started: Instant::now(),
                activity_ms: AtomicU64::new(0),
                source_validation,
                finished: AtomicBool::new(false),
                dropped: AtomicBool::new(false),
                failed: AtomicBool::new(false),
                active: AtomicUsize::new(0),
            });
            session.observe(NativeFileDragEvent::Started);
            unsafe {
                if OleInitialize(None).is_err() {
                    session.cancel();
                    PostMessageW(
                        Some(HWND(source_hwnd as *mut c_void)),
                        crate::events::WM_GPUI_NATIVE_DRAG_ENDED,
                        WPARAM(source_validation),
                        LPARAM(0),
                    )
                    .log_err();
                    log::warn!("could not initialize native file drag");
                    return;
                }
                let input = match DragInputGuard::attach(source_thread, &keyboard) {
                    Ok(input) => input,
                    Err(error) => {
                        session.cancel();
                        PostMessageW(
                            Some(HWND(source_hwnd as *mut c_void)),
                            crate::events::WM_GPUI_NATIVE_DRAG_ENDED,
                            WPARAM(source_validation),
                            LPARAM(0),
                        )
                        .log_err();
                        log::warn!("could not hand off native file drag input: {error}");
                        OleUninitialize();
                        return;
                    }
                };
                let stopped = Arc::new(AtomicBool::new(false));
                let watch_stopped = stopped.clone();
                let watch_session = Arc::downgrade(&session);
                // Cancellation must also reach a provider while this apartment is
                // servicing a synchronous Read and cannot pump COM messages.
                let watcher = std::thread::Builder::new()
                    .name("gpui-drag-cancel".into())
                    .spawn(move || {
                        while !watch_stopped.load(Ordering::Acquire) {
                            let Some(session) = watch_session.upgrade() else {
                                break;
                            };
                            if session.is_cancelled() || session.idle_expired() {
                                session.cancel();
                                break;
                            }
                            drop(session);
                            std::thread::sleep(std::time::Duration::from_millis(25));
                        }
                    });
                let watcher = match watcher {
                    Ok(watcher) => watcher,
                    Err(_) => {
                        session.cancel();
                        drop(input);
                        PostMessageW(
                            Some(HWND(source_hwnd as *mut c_void)),
                            crate::events::WM_GPUI_NATIVE_DRAG_ENDED,
                            WPARAM(source_validation),
                            LPARAM(0),
                        )
                        .log_err();
                        log::warn!("could not start native drag cancellation watcher");
                        OleUninitialize();
                        return;
                    }
                };
                let data: IDataObject = DragDataObject {
                    session: session.clone(),
                    async_mode: Cell::new(true),
                    in_operation: Cell::new(false),
                    descriptor_format: RegisterClipboardFormatW(w!("FileGroupDescriptorW")) as u16,
                    content_format: RegisterClipboardFormatW(w!("FileContents")) as u16,
                }
                .into();
                let source: IDropSource = DragSource {
                    session: session.clone(),
                }
                .into();
                let mut effect = DROPEFFECT_NONE;
                let result = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
                // Only the gesture needs shared foreground input. Retained Shell
                // streams must not keep the GPUI input queue attached during copying.
                drop(input);
                log::debug!("native file drag ended: result={result:?}, effect={effect:?}");
                PostMessageW(
                    Some(HWND(source_hwnd as *mut c_void)),
                    crate::events::WM_GPUI_NATIVE_DRAG_ENDED,
                    WPARAM(source_validation),
                    LPARAM(0),
                )
                .log_err();
                if result != DRAGDROP_S_DROP || effect == DROPEFFECT_NONE {
                    session.cancel();
                } else {
                    session.dropped.store(true, Ordering::Release);
                    session.observe(NativeFileDragEvent::Dropped);
                }
                drop(source);
                drop(data);
                // Async Shell consumers retain COM objects after DoDragDrop. Keep this
                // apartment pumping until their final Release. A stalled/leaking target
                // is cancelled after five idle minutes; active large copies have no limit.
                while Arc::strong_count(&session) > 1 && !session.is_cancelled() {
                    if session.idle_expired() {
                        session.cancel();
                        break;
                    }
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    MsgWaitForMultipleObjectsEx(None, 25, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
                stopped.store(true, Ordering::Release);
                if watcher.join().is_err() {
                    log::warn!("native drag cancellation watcher panicked");
                }
                session.finish(if session.failed.load(Ordering::Acquire) {
                    NativeFileDragOutcome::Failed
                } else if session.is_cancelled() {
                    NativeFileDragOutcome::Cancelled
                } else {
                    NativeFileDragOutcome::Provided
                });
                OleUninitialize();
            }
        })
        .is_ok()
}

pub(crate) struct DragInputGuard {
    thread: u32,
    source: u32,
}
impl DragInputGuard {
    pub(crate) fn attach(source: u32, keyboard: &[u8; 256]) -> Result<Self> {
        let thread = unsafe { GetCurrentThreadId() };
        // AttachThreadInput needs both message queues. It also resets keyboard
        // state, so restore the initiating UI thread's snapshot before OLE probes it.
        let mut message = MSG::default();
        unsafe {
            let _ = PeekMessageW(
                &mut message,
                None,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::PM_NOREMOVE,
            );
        };
        unsafe { AttachThreadInput(thread, source, true) }.ok()?;
        let guard = Self { thread, source };
        unsafe { SetKeyboardState(keyboard) }?;
        Ok(guard)
    }
}
impl Drop for DragInputGuard {
    fn drop(&mut self) {
        unsafe { AttachThreadInput(self.thread, self.source, false) }
            .ok()
            .log_err();
    }
}

#[implement(IDropSource)]
struct DragSource {
    session: Arc<DragExportSession>,
}
impl IDropSource_Impl for DragSource_Impl {
    fn QueryContinueDrag(
        &self,
        escape: BOOL,
        keys: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
    ) -> HRESULT {
        if escape.as_bool() || self.session.is_cancelled() {
            self.session.cancel();
            DRAGDROP_S_CANCEL
        } else if keys & MK_LBUTTON == windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS(0)
        {
            self.session.content_allowed.store(true, Ordering::Release);
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }
    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

#[implement(IDataObject, IDataObjectAsyncCapability)]
pub(crate) struct DragDataObject {
    pub(crate) session: Arc<DragExportSession>,
    pub(crate) async_mode: Cell<bool>,
    pub(crate) in_operation: Cell<bool>,
    pub(crate) descriptor_format: u16,
    pub(crate) content_format: u16,
}

pub(crate) fn format(id: u16, medium: TYMED, index: i32) -> FORMATETC {
    FORMATETC {
        cfFormat: id,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed: medium.0 as u32,
        ..Default::default()
    }
}

impl DragDataObject {
    fn formats(&self) -> Result<Vec<FORMATETC>> {
        let mut formats = match self.session.payload()? {
            ExternalDragPayload::Files(_) => vec![format(CF_HDROP.0, TYMED_HGLOBAL, -1)],
            ExternalDragPayload::VirtualFiles(files) => {
                let mut formats = vec![format(self.descriptor_format, TYMED_HGLOBAL, -1)];
                formats.extend(
                    files
                        .files()
                        .iter()
                        .enumerate()
                        .filter(|(_, file)| !file.is_directory)
                        .map(|(i, _)| format(self.content_format, TYMED_ISTREAM, i as i32)),
                );
                formats
            }
            _ => return Err(DV_E_FORMATETC.into()),
        };
        formats.push(format(source_marker_format(), TYMED_HGLOBAL, -1));
        Ok(formats)
    }
    fn query(&self, requested: &FORMATETC) -> HRESULT {
        if self.session.is_cancelled() {
            return STG_E_REVERTED;
        }
        if !requested.ptd.is_null() {
            return DV_E_DVTARGETDEVICE;
        }
        if requested.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        let formats = match self.formats() {
            Ok(formats) => formats,
            Err(error) => return error.code(),
        };
        if !formats.iter().any(|f| f.cfFormat == requested.cfFormat) {
            return DV_E_FORMATETC;
        }
        if !formats
            .iter()
            .any(|f| f.cfFormat == requested.cfFormat && f.lindex == requested.lindex)
        {
            return DV_E_LINDEX;
        }
        if !formats.iter().any(|f| {
            f.cfFormat == requested.cfFormat
                && f.lindex == requested.lindex
                && f.tymed & requested.tymed != 0
        }) {
            return DV_E_TYMED;
        }
        S_OK
    }
}

impl IDataObject_Impl for DragDataObject_Impl {
    fn GetData(&self, requested: *const FORMATETC) -> Result<STGMEDIUM> {
        let requested = unsafe { requested.as_ref() }
            .ok_or_else(|| windows::core::Error::from_hresult(E_POINTER))?;
        self.query(requested).ok()?;
        self.session.touch();
        if requested.cfFormat == source_marker_format() {
            let marker =
                (u128::from(std::process::id()) << 64) | self.session.source_validation as u128;
            return global_medium(&marker.to_le_bytes());
        }
        match self.session.payload()? {
            ExternalDragPayload::Files(paths) => {
                let mut names = Vec::<u16>::new();
                for (path, _) in paths.entries() {
                    let name: Vec<u16> = path.as_os_str().encode_wide().collect();
                    if name.contains(&0) || !path.is_absolute() {
                        return Err(E_INVALIDARG.into());
                    }
                    names.extend(name);
                    names.push(0);
                }
                names.push(0);
                let header = DROPFILES {
                    pFiles: size_of::<DROPFILES>() as u32,
                    fWide: true.into(),
                    ..Default::default()
                };
                let mut bytes = vec![0; size_of::<DROPFILES>() + names.len() * 2];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        &header as *const DROPFILES as *const u8,
                        bytes.as_mut_ptr(),
                        size_of::<DROPFILES>(),
                    );
                    std::ptr::copy_nonoverlapping(
                        names.as_ptr() as *const u8,
                        bytes.as_mut_ptr().add(size_of::<DROPFILES>()),
                        names.len() * 2,
                    );
                }
                global_medium(&bytes)
            }
            ExternalDragPayload::VirtualFiles(files)
                if requested.cfFormat == self.descriptor_format =>
            {
                let mut bytes = vec![0; 4 + files.files().len() * size_of::<FILEDESCRIPTORW>()];
                bytes[..4].copy_from_slice(&(files.files().len() as u32).to_le_bytes());
                for (index, file) in files.files().iter().enumerate() {
                    let mut descriptor = FILEDESCRIPTORW {
                        dwFlags: FD_ATTRIBUTES.0 as u32 | FD_PROGRESSUI.0 as u32,
                        dwFileAttributes: if file.is_directory {
                            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY.0
                        } else {
                            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL.0
                        },
                        ..Default::default()
                    };
                    let mut name = [0_u16; 260];
                    for (to, from) in name.iter_mut().zip(file.name.encode_wide()) {
                        *to = from;
                    }
                    descriptor.cFileName = name;
                    if let Some(size) = file.size {
                        descriptor.dwFlags |= FD_FILESIZE.0 as u32;
                        descriptor.nFileSizeHigh = (size >> 32) as u32;
                        descriptor.nFileSizeLow = size as u32;
                    }
                    if let Some(time) = file
                        .modified_at
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    {
                        if let Some(ticks) = (time.as_nanos() / 100)
                            .checked_add(116_444_736_000_000_000)
                            .and_then(|ticks| u64::try_from(ticks).ok())
                        {
                            descriptor.dwFlags |= FD_WRITESTIME.0 as u32;
                            descriptor.ftLastWriteTime = FILETIME {
                                dwLowDateTime: ticks as u32,
                                dwHighDateTime: (ticks >> 32) as u32,
                            };
                        }
                    }
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            &descriptor as *const FILEDESCRIPTORW as *const u8,
                            bytes
                                .as_mut_ptr()
                                .add(4 + index * size_of::<FILEDESCRIPTORW>()),
                            size_of::<FILEDESCRIPTORW>(),
                        );
                    }
                }
                global_medium(&bytes)
            }
            ExternalDragPayload::VirtualFiles(files) => {
                let descriptor = files
                    .files()
                    .get(requested.lindex as usize)
                    .filter(|file| !file.is_directory)
                    .ok_or_else(|| windows::core::Error::from_hresult(DV_E_LINDEX))?
                    .clone();
                // Opening is metadata-only. Network work begins with the first Read.
                let stream: IStream = ContentStream::new(descriptor, self.session.clone()).into();
                Ok(STGMEDIUM {
                    tymed: TYMED_ISTREAM.0 as u32,
                    u: STGMEDIUM_0 {
                        pstm: ManuallyDrop::new(Some(stream)),
                    },
                    ..Default::default()
                })
            }
            _ => Err(DV_E_FORMATETC.into()),
        }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
        Err(DV_E_FORMATETC.into())
    }
    fn QueryGetData(&self, requested: *const FORMATETC) -> HRESULT {
        unsafe { requested.as_ref() }.map_or(E_POINTER, |f| self.query(f))
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, output: *mut FORMATETC) -> HRESULT {
        if output.is_null() {
            return E_POINTER;
        }
        unsafe {
            (*output).ptd = std::ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumFormatEtc(&self, direction: u32) -> Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        unsafe { SHCreateStdEnumFmtEtc(&self.formats()?) }
    }
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn DUnadvise(&self, _: u32) -> Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

impl IDataObjectAsyncCapability_Impl for DragDataObject_Impl {
    fn SetAsyncMode(&self, value: BOOL) -> Result<()> {
        self.async_mode.set(value.as_bool());
        Ok(())
    }
    fn GetAsyncMode(&self) -> Result<BOOL> {
        Ok(self.async_mode.get().into())
    }
    fn StartOperation(&self, _: Ref<IBindCtx>) -> Result<()> {
        self.session.touch();
        self.in_operation.set(true);
        Ok(())
    }
    fn InOperation(&self) -> Result<BOOL> {
        Ok(self.in_operation.get().into())
    }
    fn EndOperation(&self, result: HRESULT, _: Ref<IBindCtx>, _: u32) -> Result<()> {
        self.in_operation.set(false);
        self.session.touch();
        if result.is_err() {
            self.session.finish(NativeFileDragOutcome::Failed);
            self.session.cancel();
        } else {
            self.session
                .finish(if self.session.failed.load(Ordering::Acquire) {
                    NativeFileDragOutcome::Failed
                } else {
                    NativeFileDragOutcome::Provided
                });
        }
        Ok(())
    }
}

fn global_medium(bytes: &[u8]) -> Result<STGMEDIUM> {
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len())?;
        let pointer = GlobalLock(global);
        if pointer.is_null() {
            GlobalFree(Some(global)).log_err();
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
        unlock_global(global);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: global },
            ..Default::default()
        })
    }
}

unsafe fn unlock_global(global: HGLOBAL) {
    // GlobalUnlock returning zero with ERROR_SUCCESS means its lock count reached
    // zero, which the generated Result wrapper can represent as Err(S_OK).
    if let Err(error) = unsafe { GlobalUnlock(global) } {
        if error.code().is_err() {
            log::warn!("could not unlock native file drag storage: {error}");
        }
    }
}

#[implement(IStream)]
pub(crate) struct ContentStream {
    descriptor: VirtualFileDescriptor,
    pub(crate) session: Arc<DragExportSession>,
    stream: RefCell<Option<Box<dyn VirtualFileStream>>>,
    offset: Cell<u64>,
}

impl ContentStream {
    pub(crate) fn new(descriptor: VirtualFileDescriptor, session: Arc<DragExportSession>) -> Self {
        Self {
            descriptor,
            session,
            stream: RefCell::new(None),
            offset: Cell::new(0),
        }
    }
}
impl Drop for ContentStream {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.get_mut() {
            stream.cancel();
        }
    }
}

impl ISequentialStream_Impl for ContentStream_Impl {
    fn Read(&self, output: *mut c_void, length: u32, read: *mut u32) -> HRESULT {
        let _operation = self.session.operation();
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        if self.session.is_cancelled() {
            return STG_E_REVERTED;
        }
        // A target may probe metadata/streams during hover. Only the release
        // that initiates a native Drop allows content to start.
        if !self.session.content_allowed.load(Ordering::Acquire) {
            return HRESULT(0x8000000A_u32 as i32); // E_PENDING
        }
        if length == 0 {
            return S_OK;
        }
        if output.is_null() {
            return E_POINTER;
        }
        let mut handle = self.stream.borrow_mut();
        if handle.is_none() {
            match self.descriptor.provider.open() {
                Ok(stream) => *handle = Some(stream),
                Err(_) => {
                    self.session.failed.store(true, Ordering::Release);
                    return STG_E_READFAULT;
                }
            }
        }
        let Some(stream) = handle.as_mut() else {
            return STG_E_READFAULT;
        };
        let mut done = 0_u32;
        while done < length {
            if self.session.is_cancelled() {
                stream.cancel();
                return STG_E_REVERTED;
            }
            let mut count = (length - done).min(VIRTUAL_FILE_CHUNK_SIZE as u32) as usize;
            if let Some(size) = self.descriptor.size {
                // Even an advertised empty file needs one source EOF request,
                // so its metadata and transfer completion are verified.
                count = count.min(size.saturating_sub(self.offset.get()).max(1) as usize);
            }
            let buffer = unsafe {
                std::slice::from_raw_parts_mut((output as *mut u8).add(done as usize), count)
            };
            self.session.touch();
            let received = match stream.read_at(self.offset.get(), buffer) {
                Ok(n)
                    if n <= count
                        && self.descriptor.size.is_none_or(|size| {
                            n as u64 <= size.saturating_sub(self.offset.get())
                        }) =>
                {
                    n
                }
                _ => {
                    self.session.failed.store(true, Ordering::Release);
                    stream.cancel();
                    return STG_E_READFAULT;
                }
            };
            if received == 0 {
                if self
                    .descriptor
                    .size
                    .is_some_and(|size| self.offset.get() < size)
                {
                    self.session.failed.store(true, Ordering::Release);
                    stream.cancel();
                    return STG_E_READFAULT;
                }
                break;
            }
            let Some(offset) = self.offset.get().checked_add(received as u64) else {
                stream.cancel();
                return STG_E_READFAULT;
            };
            self.offset.set(offset);
            done += received as u32;
            if !read.is_null() {
                unsafe {
                    *read = done;
                }
            }
        }
        if done == length { S_OK } else { S_FALSE }
    }
    fn Write(&self, _: *const c_void, _: u32, written: *mut u32) -> HRESULT {
        if !written.is_null() {
            unsafe {
                *written = 0;
            }
        }
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for ContentStream_Impl {
    fn Seek(&self, distance: i64, origin: STREAM_SEEK, position: *mut u64) -> Result<()> {
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => self.offset.get(),
            STREAM_SEEK_END => self
                .descriptor
                .size
                .ok_or_else(|| windows::core::Error::from_hresult(STG_E_INVALIDFUNCTION))?,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let next = if distance >= 0 {
            base.checked_add(distance as u64)
        } else {
            base.checked_sub(distance.unsigned_abs())
        }
        .ok_or_else(|| windows::core::Error::from_hresult(STG_E_INVALIDFUNCTION))?;
        self.offset.set(next);
        if !position.is_null() {
            unsafe {
                *position = next;
            }
        }
        Ok(())
    }
    fn Stat(&self, output: *mut STATSTG, _: &STATFLAG) -> Result<()> {
        if output.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *output = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.descriptor.size.unwrap_or(0),
                grfMode: STGM_READ,
                ..Default::default()
            };
        }
        Ok(())
    }
    fn Clone(&self) -> Result<IStream> {
        let clone = ContentStream::new(self.descriptor.clone(), self.session.clone());
        clone.offset.set(self.offset.get());
        Ok(clone.into())
    }
    fn CopyTo(
        &self,
        destination: Ref<IStream>,
        count: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> Result<()> {
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        if !written.is_null() {
            unsafe {
                *written = 0;
            }
        }
        let destination = destination
            .as_ref()
            .ok_or_else(|| windows::core::Error::from_hresult(E_POINTER))?;
        let mut buffer = vec![0; VIRTUAL_FILE_CHUNK_SIZE];
        let mut total = 0;
        while total < count {
            let mut received = 0;
            self.Read(
                buffer.as_mut_ptr() as *mut c_void,
                (count - total).min(buffer.len() as u64) as u32,
                &mut received,
            )
            .ok()?;
            if received == 0 {
                break;
            }
            let mut sent = 0;
            unsafe {
                destination
                    .Write(buffer.as_ptr() as *const c_void, received, Some(&mut sent))
                    .ok()?;
            }
            total += received as u64;
            if !read.is_null() {
                unsafe {
                    *read = total;
                }
            }
            if !written.is_null() {
                unsafe {
                    *written += sent as u64;
                }
            }
            if sent != received {
                return Err(STG_E_WRITEFAULT.into());
            }
        }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
    fn Commit(&self, _: &STGC) -> Result<()> {
        Ok(())
    }
    fn Revert(&self) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
}

/// A process/window-specific marker lets the source window restore its typed
/// drag on re-entry without pretending virtual files are local paths.
fn source_marker_format() -> u16 {
    static FORMAT: std::sync::LazyLock<u16> = std::sync::LazyLock::new(|| unsafe {
        RegisterClipboardFormatW(w!("GPUI file drag source")) as u16
    });
    *FORMAT
}
pub(crate) fn is_source_drag(data: &IDataObject, validation: usize) -> bool {
    unsafe {
        let request = format(source_marker_format(), TYMED_HGLOBAL, -1);
        let Ok(mut medium) = data.GetData(&request) else {
            return false;
        };
        let global = medium.u.hGlobal;
        let valid = if GlobalSize(global) == 16 {
            let pointer = GlobalLock(global);
            if pointer.is_null() {
                false
            } else {
                let marker = std::ptr::read_unaligned(pointer as *const u128);
                unlock_global(global);
                marker == (u128::from(std::process::id()) << 64) | validation as u128
            }
        } else {
            false
        };
        ReleaseStgMedium(&mut medium);
        valid
    }
}
