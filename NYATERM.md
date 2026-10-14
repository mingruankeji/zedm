# NyaTerm fork notes

## 2026-10-14 safe Windows internal drag targets

Internal drag routing previously read the HWND under the pointer's
`GWLP_USERDATA` as a Rust `Weak<WindowsWindowInner>`, even for another process.
Electron windows commonly populate that slot, so hovering an internal component
over VS Code could dereference a foreign address. Window lookup now upgrades
only weak references registered on the creating UI thread; `WM_NCDESTROY`
unregisters before callbacks. Routing also rejects another GPUI platform instance.
The native window procedure retains ownership of its own user-data allocation.

Validation on Windows: `cargo test -p gpui_windows --features test-support
internal_drag --lib` passed 4 tests (one additional ignored subprocess fixture
is executed by its parent test). Coverage includes hidden non-GPUI windows with
invalid nonzero user data, a real foreign-process HWND, expired/unregistered and
reused registry keys, and lookup from a different UI thread. Interactive NyaTerm
gestures are validated separately by the consuming application.

This branch carries [NyaTerm](https://github.com/nyakang/nyaterm)'s local changes
to GPUI on top of an unmodified upstream base.

- Fork: <https://github.com/nyakang/zed>
- Upstream: <https://github.com/zed-industries/zed>
- Base revision: `26363aabdb31a0c016728634202e1ad5067033aa` (upstream `main`, 2026-10-13)
- Branch: `nyaterm`
- Crates touched: `gpui`, `gpui_apple`, `gpui_wgpu`, `gpui_windows`,
  `gpui_linux`, `gpui_macos`, and `gpui_web`. Nothing else in the workspace is
  modified, and no dependency is added, so `Cargo.lock` is unchanged.

NyaTerm consumes `gpui` and the platform renderer crates from one coherent
snapshot, and needs a mutable texture it can update per dirty region for the
remote desktop surface. Without it a framebuffer update rebuilds a `RenderImage`
and clones the entire framebuffer for every frame.

## Patches

1. `feat(gpui): allocate and update mutable atlas textures` — `DynamicTexture`
   and `DynamicTextureId`, `AtlasKey::DynamicTexture`, and a new
   `PlatformAtlas::update` implemented by all five atlases (Metal, WGPU,
   DirectX, Linux headless, test). Uploads are stride-aware, so a caller can
   pass rows sliced out of a larger framebuffer, and every backend validates the
   extent, stride, payload length and destination rectangle before writing.
2. `feat(gpui): add the Window dynamic-texture API` — `create_dynamic_texture`,
   `update_dynamic_texture`, `paint_dynamic_texture` and
   `remove_dynamic_texture`.
3. `test(gpui): cover strided dirty-region uploads` — asserts a strided 2x2
   update into a 4x4 texture leaves every pixel outside the rectangle unchanged.
4. `fix(gpui_windows): release a modal dialog's disabled owner if creation fails`
   — `WindowKind::Dialog` disables its owner as soon as `GetActiveWindow`
   resolves it, and six fallible steps follow before a `WindowsWindow` exists to
   own that responsibility. Any early return left the owner disabled for the
   lifetime of the process, because `Drop for WindowsWindow` never runs and so
   the `WM_DESTROY` handler that re-enables the owner is never reached.
   `DisabledOwnerGuard` makes the handover a single step.
5. `feat(gpui_windows): warn when a Dialog has no window to own it` — an invalid
   `GetActiveWindow` left `Dialog` silently non-modal (and, without an owner,
   holding a taskbar button of its own) while still returning `Ok`.
6. `feat(gpui): add a hidden cursor style` — adds `CursorStyle::Hidden` as a
   hitbox-scoped cursor style. Windows uses a null `HCURSOR`, X11 reuses its
   persistent invisible cursor, Wayland clears the pointer surface, macOS uses
   a cached transparent `NSCursor`, and Web maps the style to CSS `none`.
7. `fix(gpui_windows): route internal drags across app windows` — while Win32
   mouse capture keeps delivering a typed GPUI drag to its source HWND, resolve
   the root GPUI window under the screen-space pointer, translate coordinates
   using that window's DPI, and deliver move/up to its existing input callback.
   Ordinary capture is unchanged because routing is gated by GPUI's active
   internal-drag state; dropping over another process or the desktop remains a
   source-window cancellation.

## Not carried here

Nothing. The old NyaTerm `vendor/zed` snapshot was byte-identical to this branch
apart from its own `NYATERM_VENDOR.md`. In particular `livekit.yaml` and
`crates/collab/.env.toml` match upstream exactly, so there is nothing
NyaTerm-local about them to keep out of this branch.

## Validation

The branch first merged upstream `63b29c2edd` after the original patch series
was based on `801c087af2`. Four conflicts required manual resolution:

- `crates/gpui/src/platform.rs` moved atlas bookkeeping into `AtlasState` and
  changed `get_or_insert_with` to take an owned key. The resolution keeps that
  model, adds a read-only tile lookup, and carries `PlatformAtlas::update` on
  the platform atlas rather than its backend.
- `crates/gpui/src/platform/test/window.rs` keeps the upstream shared headless
  atlas for ordinary test windows and retains a focused pixel-recording atlas
  for dirty-region validation.
- `crates/gpui/src/window.rs` keeps upstream visibility and text-system changes
  while adapting the dynamic-texture calls to the owned-key atlas API.
- `crates/gpui_linux/src/linux/headless/window.rs` now uses GPUI's shared
  `HeadlessAtlas`; the duplicate NyaTerm-local implementation was dropped.

Metal, WGPU, and DirectX updates were moved into their `PlatformAtlas`
implementations to match the new upstream split between atlas state and backend
allocation. The hidden-cursor and Windows dialog-owner patches merged without
content conflicts.

On Windows 11 at the new base:

```sh
cargo check -p gpui -p gpui_platform -p gpui_windows
cargo test -p gpui strided_update_preserves_pixels_outside_the_dirty_rectangle
cargo fmt -p gpui -p gpui_platform -p gpui_windows -p gpui_apple \
  -p gpui_linux -p gpui_web -p gpui_wgpu -- --check
```

All pass. Rust reports only the MSVC linker messages emitted while producing
proc-macro import libraries. `gpui_apple` (Metal) and `gpui_linux` cannot be
compiled on this host, so those two `update` implementations rest on
`.github/workflows/nyaterm.yml`, which checks `gpui_wgpu`/`gpui_linux` on Linux
and `gpui_apple` on macOS.

The hidden-cursor patch was additionally checked on Windows 11 with
`cargo check -p gpui -p gpui_platform -p gpui_windows`. Its focused
`gpui_windows` unit test is present, but the crate's test configuration currently
fails before running tests because `gpui_windows::WindowsWindow` exposes the
test-only `render_to_image` method while the selected `gpui::PlatformWindow`
trait does not. The normal library check is clean; Linux, macOS, and Web
exhaustive cursor matches are covered by the branch workflow.

The owner-disabled rollback is not covered by a test: it needs one of six Win32
calls inside `WindowsWindow::new` to fail, and none of them is injectable from
outside the function. It was reviewed against `handle_destroy_msg`
(`crates/gpui_windows/src/events.rs`), which is the only other place that
re-enables an owner, so that both paths do the same two things in the same order.

The 2026-09-22 merge to `f25434f3c5` applied without conflicts. It carries the
upstream X11 expose recovery and WGPU atlas bind-group cache changes alongside
the NyaTerm dynamic-texture implementations.

The cross-window internal-drag patch was checked on Windows 11 with:

```sh
cargo check -p gpui -p gpui_platform -p gpui_windows
cargo test -p gpui typed_drag_can_be_handed_to_another_window
cargo fmt --all -- --check
```

The focused test starts a typed drag in one GPUI window and dispatches the move
and release through a second window, asserting that the second window consumes
the payload and that the application-global drag state is cleared.

## 2026-10-01 upstream merge

Merged upstream main while preserving dynamic textures, hidden cursors, modal
owner rollback and cross-window internal drag routing. The import-list conflict
in `crates/gpui/src/window.rs` keeps the dynamic texture types and adopts
upstream `InputPreference`. Upstream atlas texture lookup now returns `Option`;
a separate compatibility patch propagates a missing-texture error on DirectX
and Metal instead of assuming a released texture exists. The README review
marker is required by this repository's `.rules` for source changes.

Windows validation with the upstream-pinned Rust 1.98.1 toolchain:

```sh
cargo check -p gpui -p gpui_platform -p gpui_windows -p gpui_wgpu
cargo test -p gpui strided_update_preserves_pixels_outside_the_dirty_rectangle
cargo test -p gpui typed_drag_can_be_handed_to_another_window
cargo fmt -p gpui -p gpui_platform -p gpui_windows -p gpui_apple \
  -p gpui_linux -p gpui_web -p gpui_wgpu -- --check
```

All passed. Metal and Linux compilation require their platform CI jobs and
were not claimed as locally verified. MSVC emits proc-macro linker messages.


## 2026-10-06 deferred file drag API

Add transport-neutral VirtualFiles descriptors with cancellable, seekable content
providers. Resolution only advertises metadata; native adapters request bounded
64 KiB ranges on workers. Safe single Unicode names and case-fold duplicates are
validated before export. Add a can_drag gesture predicate to preserve modifier
selection, and an explicit virtual-file platform capability. macOS and Wayland
retain their local Files behavior and reject VirtualFiles until their native
adapters exist. The base remains 0544bd292a52fed9718e1ba9739c4ac82c41d223.

Windows validation: cargo check -p gpui -p gpui_windows; cargo test -p gpui
--features test-support virtual --lib (4 tests); existing Files promotion/re-entry
regression passed. No new filesystem/network dependency enters GPUI. macOS and
Linux compilation are not locally verified on this Windows host.


### Windows native file exports

Implement local CF_HDROP and deferred FILEDESCRIPTORW/FILECONTENTS exports using
IDataObject, IDataObjectAsyncCapability and seekable IStream. A dedicated OLE STA
runs DoDragDrop and pumps retained async consumers; a weak cancellation watcher
handles window destruction and five minutes of inactivity even during a blocked
Read. Active transfers have no total duration limit. Content remains unavailable
while hovering. Source-window markers restore typed GPUI drags without fake local
paths; a validated window message clears suspended drag state at native completion.
Each content open is independent; stream Release cancels its provider work.

Windows validation: cargo check -p gpui -p gpui_windows; cargo test -p gpui_windows
--features test-support external_drag_tests --lib (9 tests); package formatting
checks. Tests use a local mock provider and exercise Unicode/multiple/empty files,
bounded reads, EOF/short-source errors, seeks/clones, cancellation, window lifetime,
local CF_HDROP, hover probes and source markers. Manual Explorer/native desktop
drag is NOT verified. macOS promises and Linux remote staging remain unsupported.


## 2026-10-07 native drag input handoff

The dedicated drag STA did not inherit the initiating window thread's button or
foreground input state. Attach its message queue to the source queue for the
DoDragDrop gesture and restore the source keyboard snapshot after attachment
(which resets key state). Detach immediately after DoDragDrop; retained async
content streams do not keep input queues associated. Reconcile held keys on the
validated source completion event. Trace only the HRESULT/effect, never file
paths, contents or credentials. The test platform now advertises its already
implemented virtual-file drag support for application-level gesture regressions.

Validation on Windows: cargo check -p gpui -p gpui_windows; native export suite
10 tests including a real two-thread Win32 input-queue handoff and reattachment;
GPUI virtual-drag suite 4 tests; package formatting. NyaTerm adds visible file
previews and first-press row dragging in its own views. Manual Explorer gestures
still require user validation and are not claimed by this patch.


## 2026-10-07 virtual directory and native promise contracts

Add parent-before-child virtual tree descriptors with directory attributes and
worker-only lazy enumeration. Freeze the descriptor tree (including failures)
exactly once; cancellation never enumerates it. Add transport-neutral top-level
file promises and a capability query for receiver-supplied destinations. Tree
validation rejects traversal, missing/non-directory parents, duplicate paths,
unsafe Windows components, NULs and overlong descriptor paths before advertising.

Windows validation: GPUI virtual suite, 6 tests passed; package formatting and
cargo check -p gpui -p gpui_windows passed. Provider content remains deferred.


## 2026-10-07 Windows virtual directory export

Resolve lazy trees inside the OLE STA. Emit FILE_ATTRIBUTE_DIRECTORY for empty
and non-empty directories, preserving each file's original descriptor lindex.
Only ordinary files offer FILECONTENTS streams. Explorer owns destination naming
and final writes; no destination path is invented by this adapter.

Windows validation: 11 native export tests passed, including hierarchy attributes,
empty directories, reserved content indexes and deferred enumeration/opening.
Existing cancellation, asynchronous lifetime, input handoff, seek and clone tests
remain green. Native Explorer directory gestures still require manual validation.


## 2026-10-07 Finder file promises

Offer NSFilePromiseProvider items for files and directories. Retain the weak
promise delegate through userInfo, use a dedicated NSOperationQueue for writes,
and pass Finder's final file URL into the application provider. Complete exactly
once with an NSError on provider failure or panic. Delegate release cancels the
provider and no application Entity is retained. Local NSURL drags retain their
existing behavior. CI now compiles gpui_macos as well as the Metal renderer.

Formatting passed on Windows. Native macOS compilation is delegated to the branch
CI; Finder cancellation, collision naming and live SFTP drops require a macOS
manual check and are not claimed by this commit.


## 2026-10-07 Linux local URI drag export

Add X11 Xdnd copy-source negotiation, pointer/keyboard ownership, Escape/window
cancellation, URI selection delivery, directory/multi-file support and bounded
receiver lifetime on a separate X11 connection. Finish on the foreground queue
with the same GPUI drag-exit event used by Wayland. Wayland offers copy only so
remote staging paths cannot be moved out from under a reusable prepared export.
Both backends require completed local sources; direct-save protocols are not
advertised. Formatting passed; Linux compilation runs in branch CI. Live Nautilus,
Dolphin and compositor gestures need manual validation on those desktops.


## 2026-10-07 AppKit delegate ABI follow-up

The native macOS branch compile found that define_class delegate implementations
must omit Rust-only MainThreadMarker parameters and use method_id for retained
Objective-C return values. Correct both signatures and mark NSError construction
unsafe with its fixed-domain, no-userInfo contract. Windows formatting passes;
macOS compilation is rerun in branch CI. This is an ABI correction, not a change
to the promise destination or application data contracts.


## 2026-10-07 Xdnd completion ownership

Associate the foreground completion callback with its original cancellation
handle so an old receiver completion cannot clear a newer gesture or its window
cancellation. The preceding Linux branch CI compiled gpui_linux successfully;
this guard is formatted locally and the final revision is rechecked by CI.


## 2026-10-07 native promise dispatch regression

macOS, Linux and Windows branch checks passed for the corrected delegate revision.
Add a macOS-only Objective-C dispatch test for source retention, file/directory
names, operation queue ownership, exact destination URL propagation and one-shot
success/error completion. Its provider records destinations without writing files
or requiring Finder; branch CI runs this native ABI regression. Interactive Finder
and Linux file-manager gestures remain a distinct manual verification step.


## 2026-10-07 native drag lifecycle contract

Add optional typed Started/Dropped/Finished observers for flat and deferred
virtual payloads, with a pause query for native idle accounting. Existing payload
constructors and local URI sources retain their behavior. The contract describes
content provided to a consumer, not final filesystem persistence.

Validation: rustfmt and Windows consumer compilation passed in the subsequent
platform patch tests. Finder manual tests remain pending.


## 2026-10-07 Windows delivery completion and idle accounting

Emit typed lifecycle notifications once at async EndOperation or synchronous
consumer release. Preserve read failures, report drop acceptance separately, and
exclude active reads, deferred enumeration and user pauses from idle expiry.
Window destruction and explicit cancellation still wake providers.

Validation on Windows: `cargo test -p gpui_windows --features test-support
external_drag --lib` (13 passed). This includes seeks/clones, empty files/trees,
cancellation, one-shot completion and pause/active-operation idle accounting.
Explorer interactive multi-selection and final output hashes remain pending.


## 2026-10-07 shared macOS promise queue

All NSFilePromiseProvider delegates now return the same application-wide
NSOperationQueue with at most three concurrent operations, instead of allocating
one blocking queue per item. The existing completion callback still executes
exactly once per native request. The delegate dispatch test checks the new limit;
macOS CI compiles and executes it. Local URL dragging is unchanged.

Validation: rustfmt on Windows. macOS native compilation/test results must be
confirmed by branch CI; interactive Finder directory recovery remains pending.


## 2026-10-07 failed descriptor enumeration

Preserve a deferred enumeration error as a native delivery failure even when a
synchronous target releases without calling EndOperation. Validation: Windows
export suite (13 passed); the one-shot observer test uses real COM EndOperation
dispatch. Three-platform CI passed for preceding d24d377526, including the
macOS promise delegate ABI test: run 37620647522. Final revision is rechecked.

## 2026-10-13 upstream rebase

Upstream `main` is fixed at `26363aabdb31a0c016728634202e1ad5067033aa`.
Previous published NyaTerm head: `400d43cd62b6499c77a1400d6f83116d9845e187`.
The previous branch is retained locally as `backup/nyaterm-20261013`.

The effective old-base-to-fork tree (including merge-only resolutions) was
reconstructed byte-for-byte as separate concern commits before rebasing.
Old upstream merge history was flattened; no functional net change was discarded
except the explicitly documented upstream replacements below. The concern
commits and subsequent compatibility fixes remain separate.

| Concern | Reconstructed commit on old base |
| --- | --- |
| review marker | `523976eda98b3e1d1696e85569627d8ada2a45b8` |
| deferred file and directory contracts | `d16903cbe77c5fb1804511824e786f98fcccc87e` |
| mutable atlas textures | `112f62400a11b9fcab78ecfc26ae0a960377952e` |
| hidden cursor style | `e04b6595f4caf18ec789007e57f99d809cc1cc5c` |
| Linux native file export | `e024a2007680106a5d1d813c99d8e387ca5c4b3d` |
| macOS native file promises | `af445b011afb99913bd6df6d3033beaca36ebd36` |
| Windows native file export | `2eb1f0f1fecf4d6957aeebabcf5d9dd71a26e1d3` |
| cross-window internal drags | `52329da3ff97623cc0c30de145e32caeafd4e9d7` |
| modal owner rollback | `9de264438cb763ab7e8f46dd65cbee5a0357ecd7` |
| fork validation | `18c2aa08cd1b671a3aad026f9456a0b56ddbf578` |
| fork documentation | `39a6bb8e147c1928c1cc732c265191499a2ff768` |

Replay commits before this documentation update:

- `97f32191db57b090ea41c458846b56148c129117` docs(fork): preserve review marker
- `c8ba0d7c123213480e161c5ebb67324acf09a558` feat(fork): preserve deferred file and directory contracts
- `eaed18ac230bcd3edc086a0c60b0bc5049206659` feat(fork): preserve mutable atlas textures
- `53043f67c664609dd5fd30cd70443e679618098a` feat(fork): preserve hidden cursor style
- `d269194d72419fb4572acffaaa66443b266d1453` feat(fork): preserve Linux native file export
- `df878de2eeab2a3cc5415d63d387f323d096089a` feat(fork): preserve macOS native file promises
- `5c3ddf9322f44c861304f100e1e3f8fd17473752` feat(fork): preserve Windows native file export
- `d426f0f383462721b09a0272035a58dd7927d389` feat(fork): preserve cross-window internal drags
- `fd47c978b98e8e0d53acd385f88a396bd347012c` feat(fork): preserve modal owner rollback
- `748c0722a50db3004c15a5ffc50b78d0909fc418` ci(fork): preserve fork validation
- `cae68287b853719ed505ee3366b1a67e95679c67` docs(fork): preserve fork documentation
- `c2160fabe920c7c4e3206eee9b8f80a243e26a06` fix(gpui): adapt native drag patches to upstream platform changes
- `5db7a2ddeb4cdec0a2f772b20d922b8385b8f7ef` style(gpui): order combined virtual-file exports

Retained dynamic textures, hidden cursors, modal owner rollback, cross-window
typed drags, deferred file/directory contracts, native Windows exports, AppKit
file promises and Linux Xdnd export with the previous lifecycle limits.
Upstream now assigns WM_USER+10 to display changes, so native-drag completion
moves to WM_USER+11. Upstream DisplayId/TestDisplay imports stay present; obsolete
end-session/X11 destroyed state is not resurrected. The only additional style
change orders the combined GPUI public imports.

Windows checks for GPUI, platform, Windows and WGPU passed; strided dirty-region
upload (1), virtual contract suite (6), cross-window handoff (1), and native
external drag suite (13) passed. Scoped fmt for all eight modified GPUI/platform
packages passed. Branch CI retains native macOS promises and Linux backend checks.


## 2026-10-14 interrupted pointer gestures

Add `PointerCancelEvent`, `PointerCancelReason::{CaptureLost, CancelMode}` and
`PlatformInput::PointerCancelled` to the window input listeners. Cancellation
clears GPUI's drag, preview, hitbox capture and pending element clicks before
notifying application listeners, without synthesizing mouse-up or accepting a
drop. `App::has_internal_drag()` excludes native file drag ownership so consumers
can handle Esc without interfering with OLE export.

Windows classifies capture release as normal mouse-up, native drag handoff or
explicit cancellation. Unexpected WM_CAPTURECHANGED/WM_CANCELMODE notifications
are posted after the native callback with a window validation value and a gesture
generation shared by the platform's windows. Duplicates and old notifications
cannot cancel a later gesture, including one started in another window. A new
mouse press clears the previous internal drag if its deferred cancellation has
not yet arrived. WM_CANCELMODE still reaches DefWindowProc for native cleanup.

Windows validation: GPUI drag lifecycle suite (9 passed), native pointer
cancellation state tests (5 passed), and gpui_windows/gpui_platform check passed.
The full Windows native suite passed 42 tests (one subprocess fixture ignored
by the harness and exercised by its parent); scoped gpui/gpui_windows fmt passed.
Branch CI now covers safe HWND queries, cancellation and GPUI drag lifecycle
alongside the existing native file export suite. Interactive external application
dragging, mixed DPI and live remote exports remain pending manual verification.

## 2026-10-14 cancellation wins over queued button-up

Reject a mouse-up whose gesture already has a pending unexpected capture
cancellation. Windows can have queued the button-up before the posted cancel
notification, so treating it as an ordinary release would accept a drop or
detach a tab that was already cancelled. Preserve the queued notification and
its generation instead; ordinary releases and OLE handoff remain unchanged.

Windows validation: pointer cancellation suite (6 passed), gpui_windows check
and scoped fmt passed. The added state regression preserves the once-only
notification through the late-up release path.

Validation is on Windows; native macOS/Linux checks are delegated to the
branch workflow. Interactive GUI/file-drop and real-server authentication
checks were not performed by this update. Their outcomes must not be inferred
from scripted peers or compile checks.
