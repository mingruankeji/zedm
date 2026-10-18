use std::cell::Cell;

use gpui_util::ResultExt as _;
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled},
        WindowsAndMessaging::{
            GW_OWNER, GetForegroundWindow, GetWindow, IsChild, IsWindow, SW_HIDE,
            SetForegroundWindow, ShowWindowAsync,
        },
    },
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum RestoreStage {
    Close,
    Drop,
    Destroy,
}

/// Owns the obligation to re-enable the HWND disabled by a modal GPUI window.
pub(crate) struct ModalOwner(Cell<Option<HWND>>);

impl ModalOwner {
    pub(crate) fn new(owner: Option<HWND>) -> Self {
        Self(Cell::new(owner))
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.0.get().is_some()
    }

    pub(crate) fn hide(&self, dialog: HWND) {
        // A hidden foreground dialog cannot hand activation back reliably while
        // its owner is still disabled. Keep this order shared by every Drop.
        self.release(dialog, RestoreStage::Drop);
        unsafe { ShowWindowAsync(dialog, SW_HIDE).ok().log_err() };
    }

    pub(crate) fn close_requested(&self, dialog: HWND, accepted: bool) -> bool {
        if accepted {
            self.release(dialog, RestoreStage::Close);
        }
        accepted
    }

    pub(crate) fn release(&self, dialog: HWND, stage: RestoreStage) {
        // EnableWindow and activation synchronously dispatch messages. Consume the
        // obligation first, so reentrant destruction cannot restore it twice.
        let Some(owner) = self.0.take() else {
            return;
        };
        let foreground = unsafe { GetForegroundWindow() };
        let valid = unsafe { IsWindow(Some(owner)).as_bool() };
        let enabled = unsafe { IsWindowEnabled(owner).as_bool() };
        log::debug!(
            "modal owner restore: stage={stage:?} dialog={dialog:?} owner={owner:?} valid={valid} enabled={enabled} foreground={foreground:?}"
        );
        if !valid {
            return;
        }

        let restore_foreground = !matches!(stage, RestoreStage::Destroy)
            && foreground_belongs_to_dialog(dialog, foreground);
        let _previously_disabled = unsafe { EnableWindow(owner, true) };
        if !unsafe { IsWindowEnabled(owner).as_bool() } {
            log::warn!(
                "modal owner remains disabled: stage={stage:?} dialog={dialog:?} owner={owner:?}"
            );
            // Leave the destroy fallback a chance to release the owner if the
            // first attempt failed. Successful restoration is always one-shot.
            self.0.set(Some(owner));
            return;
        }

        // Re-check after EnableWindow: a synchronous handler may have activated
        // another application. Closing a background dialog must not steal focus.
        let foreground_before_activation = unsafe { GetForegroundWindow() };
        let activation_requested = restore_foreground
            && foreground_belongs_to_dialog(dialog, foreground_before_activation);
        let activated = activation_requested && unsafe { SetForegroundWindow(owner).as_bool() };
        let foreground_after = unsafe { GetForegroundWindow() };
        log::debug!(
            "modal owner restored: stage={stage:?} dialog={dialog:?} owner={owner:?} enabled=true foreground_before_activation={foreground_before_activation:?} activation_requested={activation_requested} activated={activated} foreground_after={foreground_after:?}"
        );
        if activation_requested && !activated && foreground_after != owner {
            log::warn!(
                "modal owner foreground restore failed: stage={stage:?} dialog={dialog:?} owner={owner:?} foreground={foreground_after:?}"
            );
        }
    }
}

fn foreground_belongs_to_dialog(dialog: HWND, mut foreground: HWND) -> bool {
    if dialog.is_invalid() {
        return false;
    }
    while !foreground.is_invalid() {
        if foreground == dialog || unsafe { IsChild(dialog, foreground).as_bool() } {
            return true;
        }
        foreground = match unsafe { GetWindow(foreground, GW_OWNER) } {
            Ok(owner) => owner,
            Err(_) => break,
        };
    }
    false
}

#[cfg(test)]
mod tests {
    use crate::modal_owner::{ModalOwner, RestoreStage, foreground_belongs_to_dialog};
    use std::cell::Cell;
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            UI::{
                Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled},
                Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
                WindowsAndMessaging::{
                    CreateWindowExW, DestroyWindow, DispatchMessageW, GW_OWNER,
                    GetForegroundWindow, GetWindow, IsWindowVisible, MSG, PM_REMOVE, PeekMessageW,
                    SW_SHOWNOACTIVATE, ShowWindow, WINDOW_EX_STYLE, WM_SHOWWINDOW, WS_CHILD,
                    WS_OVERLAPPED,
                },
            },
        },
        core::w,
    };

    struct NativeWindow(HWND);

    impl NativeWindow {
        fn new(owner: Option<HWND>, child: bool) -> Self {
            let style = if child { WS_CHILD } else { WS_OVERLAPPED };
            Self(
                unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE::default(),
                        w!("STATIC"),
                        w!(""),
                        style,
                        0,
                        0,
                        1,
                        1,
                        owner,
                        None,
                        None,
                        None,
                    )
                }
                .expect("create modal lifecycle test window"),
            )
        }
    }

    impl Drop for NativeWindow {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.0) }.expect("destroy modal lifecycle test window");
        }
    }

    #[test]
    fn modal_owner_is_enabled_before_the_closing_dialog_is_hidden() {
        let owner = NativeWindow::new(None, false);
        let dialog = NativeWindow::new(Some(owner.0), false);
        unsafe {
            let _previously_visible = ShowWindow(dialog.0, SW_SHOWNOACTIVATE);
            let _previously_disabled = EnableWindow(owner.0, false);
        }
        let enabled_when_hidden = Cell::new(None);
        unsafe extern "system" fn observe_hide(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
            _subclass_id: usize,
            reference: usize,
        ) -> LRESULT {
            if message == WM_SHOWWINDOW && wparam.0 == 0 {
                let enabled = unsafe { &*(reference as *const Cell<Option<bool>>) };
                let owner = unsafe { GetWindow(hwnd, GW_OWNER) }.expect("dialog owner");
                enabled.set(Some(unsafe { IsWindowEnabled(owner).as_bool() }));
            }
            unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
        }
        assert!(unsafe {
            SetWindowSubclass(
                dialog.0,
                Some(observe_hide),
                1,
                &enabled_when_hidden as *const _ as usize,
            )
            .as_bool()
        });
        let modal = ModalOwner::new(Some(owner.0));
        modal.hide(dialog.0);
        // ShowWindowAsync queues the visibility change even on this thread.
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() } {
            unsafe { DispatchMessageW(&message) };
        }
        assert!(unsafe { RemoveWindowSubclass(dialog.0, Some(observe_hide), 1).as_bool() });
        assert!(unsafe { IsWindowEnabled(owner.0).as_bool() });
        assert!(!unsafe { IsWindowVisible(dialog.0).as_bool() });
        assert_eq!(enabled_when_hidden.get(), Some(true));
    }

    #[test]
    fn modal_owner_is_not_released_twice_by_close_drop_and_destroy() {
        let owner = NativeWindow::new(None, false);
        let dialog = NativeWindow::new(Some(owner.0), false);
        let _previously_disabled = unsafe { EnableWindow(owner.0, false) };
        let modal = ModalOwner::new(Some(owner.0));
        assert!(modal.close_requested(dialog.0, true));
        assert!(unsafe { IsWindowEnabled(owner.0).as_bool() });
        // Another modal may disable the owner before this one's deferred Drop.
        let _previously_disabled = unsafe { EnableWindow(owner.0, false) };
        modal.release(dialog.0, RestoreStage::Drop);
        modal.release(dialog.0, RestoreStage::Destroy);
        assert!(!unsafe { IsWindowEnabled(owner.0).as_bool() });
        let _previously_disabled = unsafe { EnableWindow(owner.0, true) };
    }

    #[test]
    fn rejecting_close_keeps_the_owner_disabled_until_an_accepted_close() {
        let owner = NativeWindow::new(None, false);
        let dialog = NativeWindow::new(Some(owner.0), false);
        let _previously_disabled = unsafe { EnableWindow(owner.0, false) };
        let modal = ModalOwner::new(Some(owner.0));
        assert!(!modal.close_requested(dialog.0, false));
        assert!(!unsafe { IsWindowEnabled(owner.0).as_bool() });
        assert!(modal.close_requested(dialog.0, true));
        assert!(unsafe { IsWindowEnabled(owner.0).as_bool() });
    }

    #[test]
    fn destroyed_and_missing_owners_are_safe_to_release() {
        let owner = NativeWindow::new(None, false);
        let modal = ModalOwner::new(Some(owner.0));
        drop(owner);
        modal.release(HWND::default(), RestoreStage::Drop);
        assert!(modal.0.get().is_none());
        ModalOwner::new(None).release(HWND::default(), RestoreStage::Destroy);
    }

    #[test]
    fn background_close_and_destroy_fallback_leave_the_foreground_unchanged() {
        let owner = NativeWindow::new(None, false);
        let dialog = NativeWindow::new(Some(owner.0), false);
        let foreground = unsafe { GetForegroundWindow() };
        assert!(!foreground_belongs_to_dialog(dialog.0, foreground));
        for stage in [RestoreStage::Drop, RestoreStage::Destroy] {
            let _previously_disabled = unsafe { EnableWindow(owner.0, false) };
            ModalOwner::new(Some(owner.0)).release(dialog.0, stage);
            assert!(unsafe { IsWindowEnabled(owner.0).as_bool() });
            assert_eq!(unsafe { GetForegroundWindow() }, foreground);
        }
    }

    #[test]
    fn only_the_dialog_its_children_and_its_owned_windows_qualify_for_activation() {
        let owner = NativeWindow::new(None, false);
        let dialog = NativeWindow::new(Some(owner.0), false);
        let child = NativeWindow::new(Some(dialog.0), true);
        let nested = NativeWindow::new(Some(dialog.0), false);
        let sibling = NativeWindow::new(Some(owner.0), false);
        for window in [dialog.0, child.0, nested.0] {
            assert!(foreground_belongs_to_dialog(dialog.0, window));
        }
        for window in [owner.0, sibling.0, HWND::default()] {
            assert!(!foreground_belongs_to_dialog(dialog.0, window));
        }
    }
}
