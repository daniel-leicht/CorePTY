# Changelog

## 0.5.0 (2026-11-05)

### New: restore your tabs

- CorePTY keeps a snapshot of its open tabs (every 30 seconds, shortly after a
  change, and as the window closes) and offers to reopen them at startup, from
  the `+` menu, or from the empty window. Local shells reopen in their
  folders; tabs that ran Claude Code resume the same session with the same
  options (`claude --resume <id> --dangerously-skip-permissions …`), unless
  it's open elsewhere. Saved SSH and Telnet connections reconnect, file
  managers reopen their local folders, and pinned tab names come back. The
  dialog lists what each tab will run, and any of them can be left out.
- PowerShell reports its folder at each prompt (OSC 7) while the feature is on.
- Settings › Restore tabs turns it off.
- A connection saved from the connection dialog is linked to its tab (Open
  SFTP finds the tab's connection, and the tab can be restored).

### Fixes

- Copy-on-select no longer copies an old selection again when you click back
  into the terminal (on its margin or scrollbar, after switching tabs or
  windows), overwriting what you had copied elsewhere since; only a selection
  just made is copied.

Fixes from a review of 0.4.0's remote editing and opening:

- Opening a remote file with `Enter` asks first unless its type is one known
  to be safe to open; 0.4.0 asked only for a list of program and script types,
  which missed some an interpreter or client runs (`.pyc`, `.rdp`,
  `.diagcab`, …).
- On Linux, `F4` with no editor set opens the desktop's text editor, not the
  file type's app (which could run a `.jar` or `.desktop` file); on Windows,
  only known-safe types use their own editor, others open in Notepad.
- An upload cut off while writing the server's file in place is redone
  without asking, instead of being taken for someone else's change.
- A failed upload is retried every 15 seconds (and when the connection comes
  back), not re-sent on every check; a server that won't set permissions gets
  the file written in place instead of no upload at all.
- An edited file's permissions are set before its contents go up, so a
  private file is never readable on the way.
- Closing the app counts a save made just before, and lets a running upload
  finish; its question says where the copies are. A copy opened with `Enter`
  and changed is kept when its tab closes.
- A folder that appears where an edited file was is never replaced; a
  temporary file a dropped upload leaves on the server is removed later.
- Edit conflicts wait while another question is open, and putting one off
  (Esc) no longer counts as keeping the server's version; the Properties
  dialog no longer offers changes it can't make to files of unknown mode.
- Reconnecting with a login for another server is refused before it logs in;
  editors started through a `.cmd` file get no console window.

## 0.4.0 (2026-11-04)

### New: a dual-pane file manager, local and SFTP

- A Commander-style file manager tab (`Ctrl+Shift+E`, in the spirit of Norton
  Commander and WinSCP). Each panel shows this machine or an SFTP server, and
  files copy or move between any two: local to remote, remote to local, or
  server to server.
- SFTP on the connection an SSH tab already has ("Open SFTP" in the tab's
  menu: no second login), or on a saved connection (its keychain secret, or a
  password prompt). The location menus (`Alt+F1` / `Alt+F2`) list this
  machine, the panel's server, SSH tabs, and saved connections. A remote panel
  can follow the terminal's working folder (for shells that report it with
  OSC 7).
- A background transfer queue with progress, speed, and time left. Every name
  clash is asked about, nested ones included (overwrite, skip, keep both,
  newer only, or the same for all). Files of 1 MiB and more resume from a
  `.filepart` after an interruption, and jobs pause while a connection is down
  and carry on after Reconnect.
- Remote files: `F3` views text and images in place, `Enter` opens a
  downloaded copy (a program or script asks first), and `F4` edits one in
  your editor (Settings › File manager) and uploads every save, after checking
  the server's copy didn't change, keeping its permissions. A save that can't
  be uploaded is tried again; a copy the server is missing changes from is
  kept when its tab closes, and closing the app asks first.
- Properties (`Alt+Enter`): size, dates, owner, and a permissions editor
  (chmod, optionally for everything inside a folder).
- Keyboard-first marking kept apart from the cursor (`Insert`, `Space`,
  `Shift`+movement, numpad masks), quick search by typing, filters, history,
  sortable columns, drag and drop, a clipboard, and Trash/Recycle Bin for local
  files.
- Big folders stay fast: only the visible rows are drawn. Remote names are
  kept byte for byte; names Windows can't store (`a:b`, `CON`) are saved under
  safe spellings.

### Fixes

- With many tabs open, every tab is reachable again: the strip scrolls (also
  with the mouse wheel), has scroll buttons and an all-tabs list, and always
  reveals the active tab.

### Under the hood

- CorePTY's own SFTP v3 client on `russh`: pipelined reads and writes, file
  data on a channel of its own, SSH connections shared between terminal tabs
  and file panels.
- Dependency updates for new security advisories.
