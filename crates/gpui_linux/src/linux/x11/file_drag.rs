use super::XcbAtoms;
use anyhow::{Result, bail};
use gpui::FileDragPaths;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            self, AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as _, EventMask,
            GrabMode, GrabStatus, SelectionNotifyEvent,
        },
    },
    wrapper::ConnectionExt as _,
};

/// A separate connection owns the pointer grab and selection while the GPUI
/// event loop stays responsive. Only completed local URI sources enter here.
pub(crate) fn run(
    paths: FileDragPaths,
    source_window: u32,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let bytes = paths
        .entries()
        .iter()
        .map(|(path, _)| {
            url::Url::from_file_path(path)
                .map(|url| format!("{url}\r\n"))
                .map_err(|_| anyhow::anyhow!("invalid drag URI"))
        })
        .collect::<Result<String>>()?
        .into_bytes();
    if bytes.is_empty() || bytes.len() > 64 * 1024 {
        bail!("invalid drag URI payload size");
    }
    let (connection, screen) = x11rb::connect(None)?;
    let root = connection.setup().roots[screen].root;
    let source = connection.generate_id()?;
    let atoms = XcbAtoms::new(&connection)?.reply()?;
    connection
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            source,
            root,
            -1,
            -1,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
    connection.map_window(source)?.check()?;
    connection
        .set_selection_owner(source, atoms.XdndSelection, x11rb::CURRENT_TIME)?
        .check()?;
    let grab = connection
        .grab_pointer(
            false,
            root,
            EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        )?
        .reply()?;
    if grab.status != GrabStatus::SUCCESS {
        bail!("native drag pointer grab failed");
    }
    // Keyboard grab makes Escape cancel without dispatching it into the editor.
    let keyboard = connection
        .grab_keyboard(
            false,
            root,
            x11rb::CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )?
        .reply()?;
    if keyboard.status != GrabStatus::SUCCESS {
        bail!("native drag keyboard grab failed");
    }
    connection.flush()?;
    if !connection
        .query_pointer(root)?
        .reply()?
        .mask
        .contains(xproto::KeyButMask::BUTTON1)
    {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut target = 0;
    let mut accepted = false;
    let mut dropped = false;
    let mut drop_deadline = deadline;
    loop {
        if cancelled.load(Ordering::Acquire)
            || Instant::now() > deadline
            || Instant::now() > drop_deadline
        {
            if target != 0 && !dropped {
                send(&connection, target, atoms.XdndLeave, [source, 0, 0, 0, 0])?;
            }
            break;
        }
        while let Some(event) = connection.poll_for_event()? {
            match event {
                Event::ClientMessage(event)
                    if event.type_ == atoms.XdndStatus && event.data.as_data32()[0] == target =>
                {
                    accepted = event.data.as_data32()[1] & 1 != 0
                        && event.data.as_data32()[4] == atoms.XdndActionCopy;
                }
                Event::ClientMessage(event)
                    if dropped
                        && event.type_ == atoms.XdndFinished
                        && event.data.as_data32()[0] == target =>
                {
                    return Ok(());
                }
                Event::SelectionRequest(event) if event.selection == atoms.XdndSelection => {
                    let property = if event.property == 0 {
                        event.target
                    } else {
                        event.property
                    };
                    let supplied = if event.target == atoms.TextUriList {
                        connection
                            .change_property8(
                                xproto::PropMode::REPLACE,
                                event.requestor,
                                property,
                                atoms.TextUriList,
                                &bytes,
                            )?
                            .check()?;
                        property
                    } else {
                        0
                    };
                    connection
                        .send_event(
                            false,
                            event.requestor,
                            EventMask::NO_EVENT,
                            SelectionNotifyEvent {
                                response_type: xproto::SELECTION_NOTIFY_EVENT,
                                sequence: 0,
                                time: event.time,
                                requestor: event.requestor,
                                selection: event.selection,
                                target: event.target,
                                property: supplied,
                            },
                        )?
                        .check()?;
                }
                Event::KeyPress(event) => {
                    let mapping = connection.get_keyboard_mapping(event.detail, 1)?.reply()?;
                    if mapping.keysyms.contains(&0xff1b) {
                        if target != 0 {
                            send(&connection, target, atoms.XdndLeave, [source, 0, 0, 0, 0])?;
                        }
                        return Ok(());
                    }
                }
                Event::ButtonRelease(event) if event.detail == 1 && !dropped => {
                    connection.ungrab_pointer(event.time)?.check()?;
                    connection.ungrab_keyboard(event.time)?.check()?;
                    if target == 0 || !accepted {
                        if target != 0 {
                            send(&connection, target, atoms.XdndLeave, [source, 0, 0, 0, 0])?;
                        }
                        return Ok(());
                    }
                    send(
                        &connection,
                        target,
                        atoms.XdndDrop,
                        [source, 0, event.time, 0, 0],
                    )?;
                    dropped = true;
                    drop_deadline = Instant::now() + Duration::from_secs(60);
                }
                _ => {}
            }
        }
        if !dropped {
            let pointer = connection.query_pointer(root)?.reply()?;
            let next = find_target(&connection, root, source_window, source, atoms.XdndAware)?;
            if next != target {
                if target != 0 {
                    send(&connection, target, atoms.XdndLeave, [source, 0, 0, 0, 0])?;
                }
                target = next;
                accepted = false;
                if target != 0 {
                    send(
                        &connection,
                        target,
                        atoms.XdndEnter,
                        [source, 5 << 24, atoms.TextUriList, 0, 0],
                    )?;
                }
            }
            if target != 0 {
                let position =
                    ((pointer.root_x as u16 as u32) << 16) | pointer.root_y as u16 as u32;
                send(
                    &connection,
                    target,
                    atoms.XdndPosition,
                    [
                        source,
                        0,
                        position,
                        x11rb::CURRENT_TIME,
                        atoms.XdndActionCopy,
                    ],
                )?;
            }
        }
        connection.flush()?;
        std::thread::sleep(Duration::from_millis(10));
    }
    // Disconnect releases both grabs, selection ownership and the source window.
    Ok(())
}

fn send(connection: &impl Connection, target: u32, atom: u32, data: [u32; 5]) -> Result<()> {
    connection
        .send_event(
            false,
            target,
            EventMask::NO_EVENT,
            ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                window: target,
                type_: atom,
                data: ClientMessageData::from(data),
            },
        )?
        .check()?;
    Ok(())
}
fn find_target(
    connection: &impl Connection,
    root: u32,
    source_window: u32,
    source: u32,
    aware: u32,
) -> Result<u32> {
    let mut current = root;
    let mut found = 0;
    for _ in 0..64 {
        let child = connection.query_pointer(current)?.reply()?.child;
        if child == 0 || child == source {
            break;
        }
        if child == source_window {
            return Ok(0);
        }
        let property = connection
            .get_property(false, child, aware, AtomEnum::ATOM, 0, 1)?
            .reply()?;
        if property
            .value32()
            .and_then(|mut values| values.next())
            .is_some_and(|version| version >= 3)
        {
            found = child;
        }
        current = child;
    }
    Ok(found)
}
