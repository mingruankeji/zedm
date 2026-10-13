use crate::{
    App, AppContext, Context, Empty, ExternalDragPayload, InputEvent, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render,
    StatefulInteractiveElement, Styled, TestAppContext, VirtualFileDescriptor,
    VirtualFileDragPayload, VirtualFileProvider, VirtualFileStream, Window, div, point, px,
};
use std::{
    cell::Cell,
    io,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Source(Arc<AtomicUsize>);
impl VirtualFileProvider for Source {
    fn open(&self) -> io::Result<Box<dyn VirtualFileStream>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(io::ErrorKind::Unsupported.into())
    }
    fn cancel(&self) {}
}
struct DragView {
    resolutions: Rc<Cell<usize>>,
    opens: Arc<AtomicUsize>,
}
impl Render for DragView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let resolutions = self.resolutions.clone();
        let opens = self.opens.clone();
        div()
            .id("virtual-drag")
            .size_full()
            .on_drag((), |_, _, _, cx| cx.new(|_| Empty))
            .can_drag(|event, _, _| !event.modifiers.shift)
            .external_drag_payload(move |_: &(), _: &mut Window, _: &mut App| {
                resolutions.set(resolutions.get() + 1);
                Some(ExternalDragPayload::VirtualFiles(
                    VirtualFileDragPayload::new([VirtualFileDescriptor {
                        is_directory: false,
                        name: "你好.txt".into(),
                        size: Some(0),
                        modified_at: None,
                        provider: Arc::new(Source(opens.clone())),
                    }])
                    .unwrap(),
                ))
            })
    }
}

#[crate::test]
fn virtual_resolver_runs_once_only_at_promotion_and_never_requests_content(
    cx: &mut TestAppContext,
) {
    for promote in [false, true] {
        let resolutions = Rc::new(Cell::new(0));
        let opens = Arc::new(AtomicUsize::new(0));
        let window = cx.add_window({
            let resolutions = resolutions.clone();
            let opens = opens.clone();
            move |_, _| DragView { resolutions, opens }
        });
        cx.test_window(window.into())
            .set_start_external_drag_result(false);
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_event(
                MouseDownEvent {
                    position: point(px(10.), px(10.)),
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseMoveEvent {
                    position: point(px(20.), px(20.)),
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Default::default(),
                }
                .to_platform_input(),
                cx,
            );
            assert!(cx.has_active_drag());
            assert_eq!(resolutions.get(), 0);
            if promote {
                for x in [-1., -2.] {
                    window.dispatch_event(
                        MouseMoveEvent {
                            position: point(px(x), px(20.)),
                            pressed_button: Some(MouseButton::Left),
                            modifiers: Default::default(),
                        }
                        .to_platform_input(),
                        cx,
                    );
                }
            }
            window.dispatch_event(
                MouseUpEvent {
                    position: point(px(20.), px(20.)),
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        assert_eq!(resolutions.get(), usize::from(promote));
        assert_eq!(opens.load(Ordering::SeqCst), 0);
        assert_eq!(
            cx.test_window(window.into())
                .external_drag_virtual_names()
                .len(),
            usize::from(promote)
        );
    }
}

#[crate::test]
fn drag_predicate_leaves_modifier_selection_gestures_unclaimed(cx: &mut TestAppContext) {
    let resolutions = Rc::new(Cell::new(0));
    let opens = Arc::new(AtomicUsize::new(0));
    let window = cx.add_window(move |_, _| DragView { resolutions, opens });
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        let modifiers = crate::Modifiers {
            shift: true,
            ..Default::default()
        };
        window.dispatch_event(
            MouseDownEvent {
                position: point(px(10.), px(10.)),
                button: MouseButton::Left,
                modifiers,
                click_count: 1,
                first_mouse: false,
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseMoveEvent {
                position: point(px(20.), px(20.)),
                pressed_button: Some(MouseButton::Left),
                modifiers,
            }
            .to_platform_input(),
            cx,
        );
        assert!(!cx.has_active_drag());
    })
    .unwrap();
}
