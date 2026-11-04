# Changelog

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
