# UI/UX implementation progress

This implements the safety and everyday-workflow portions of the audit, plus
navigation, search, accessibility and progress improvements. The remaining work
below is required before calling the full redesign complete.

## Implemented

- Trash failures leave sources intact. Permanent deletion and Empty Trash require
  confirmation with Cancel as the default.
- `trash://` displays deleted files, preserving separate versions with the same
  original name. It reads the home trash and supported trash directories on
  mounted filesystems on a cancellable worker. Sidecars are bounded, validated,
  and paired with their stored filenames. Original location and deletion date
  appear in preview.
- Restore recreates missing parents, preserves existing originals with numbered
  names, and removes the restored item's sidecar. Restore and Empty Trash have
  visible collection controls. Other open Trash windows refresh after operations.
- Copy/cut publishes desktop file lists, URI lists and the conventional
  copied-files format. Paste reads the current desktop clipboard, including
  across windows and other compatible applications, with bounded input and
  off-thread metadata loading. Cut items are marked; completed moves prune only
  clipboard data still owned by the application.
- Native drag sources and drop targets support file views, folder cells and
  sidebar folders, using the existing operation/conflict pipeline. The default
  is Copy; Shift requests Move. Archives and Trash reject mutations.
- Connect reads the entered password into a Secret, prevents duplicate attempts,
  retains non-secret fields on failure and reports pending and failed attempts.
  Protocol-specific password labels and persistence controls follow visibility.
- Errors and operation results appear inside the originating window, with an Undo
  button when available. Desktop notifications remain available too.
- Selection follows file identity through refresh, sort and clipboard repaint in
  grid, list and column views. Unchanged sidebar rows are retained during device
  polling rather than rebuilt, preserving focus.
- Recent, Starred and Trash metadata is loaded off the UI thread. Navigation
  cancels collection work and discards obsolete replies.
- Ctrl+F filters current-folder names; Ctrl+Shift+F searches the index. Scope and
  status are visible, breadcrumbs remain available, and Load More retrieves and
  accumulates additional indexed pages. Hidden filter callbacks cannot replace
  collection content or status; Ctrl+L dismisses filtering before editing a path.
- Header controls expose sidebar, preview and New Folder. Compact overlays close
  with Escape and return focus to the listing. Compact headers keep view choices
  available through the menu.
- View > Sort By works in every mode; sort controls accept keyboard focus and
  view buttons expose selection. Main-menu Star/Unstar and Bookmark/Remove labels
  follow current state when opened.
- Operations exposes each job and individual cancellation, alongside Cancel All.
  Unknown totals pulse instead of showing a fake percentage.
- Loading, empty-folder, no-matches and failure presentations include contextual
  New Folder, Clear Search or Retry actions. Listings have scroll containers.
  Form labels expose field relationships, status labels have a status role and
  text sizes follow the theme.

## Remaining acceptance work and limits

1. Restore currently preserves both versions automatically on collision. A
   restore-specific conflict chooser with Skip/Replace is still needed.
2. Drag-and-drop deliberately defaults to Copy on every filesystem. Automatic
   same-filesystem Move/cross-filesystem Copy policy remains to be implemented;
   Shift explicitly requests Move today. Pointer gestures need manual review.
3. Local filtering and indexed search remain separate implementations, though
   mutually exclusive and displayed beneath the breadcrumb header. Full unification
   into one scope selector remains.
4. Foreground operations still send desktop notifications as well as inline
   feedback. Background-only notification policy remains.
5. Finish the full theme, high-contrast, large-text, scale, compact-window and
   screen-reader review. Further action sensitivity for filesystem permissions
   needs validation against read-only volumes and unavailable shares.

## Verification

- Core location/model suite: 82 tests passed, including trash:// round-tripping.
- Required-display GTK suite: 408 tests passed with desktop-icons, and 396 tests
  passed with the default feature set.
- Regression coverage includes native clipboard repeat reads, cut state and
  escaped names, selection identity, individual job cancellation, pagination,
  hidden-filter navigation, duplicate trash origins, and restore collisions and
  missing parents.
- The complete headless UI smoke suite passes, including context menus, repeated
  copy/paste, multi-window navigation, Trash/Restore bytes, dropped-file copying,
  thumbnails, previews, network rows and desktop icons. GTK's seven known teardown
  warnings are ignored by the existing smoke policy.
- The smoke harness owns a private D-Bus session and cache so accessibility-bus
  sockets cannot collide with other test sessions. Copy is exercised through real
  Ctrl+C input to establish Wayland clipboard ownership.
- Formatting, whitespace, Python syntax and shell syntax checks pass.
- The Computer tests now explicitly select a nonexistent Desktop fixture rather
  than depending on the real home directory's contents.
- The earlier unrelated default-workspace mount-helper device-validation failure
  in the SDK's root environment has not been changed or certified by this work.
