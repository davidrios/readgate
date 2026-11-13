# readgate

readgate makes a program ask you before it can read a file, or write it
when you let the file be written.

You give it a source file, a mount folder, and a dest path inside that
folder. readgate mounts the folder through FUSE and shows the source at the
dest. When a program reads the dest, readgate holds the read and shows you
a dialog with
everything it can find out about the reader: the executable, its command
line, its user, its working folder and the processes that started it. If
you allow it, the read goes through. If you deny it, the reader gets an
ordinary I/O error. Either way the reader sees a plain file, and to it the
read only took a while.

The point is sharing a file with another user on the same machine without
handing it over. readgate runs as you and reads the source as you. The other
user's programs only ever see the dest, and each read waits on your answer.
Every answer goes to a log file.

A server of readgate's holds the mounts and asks the questions, and keeps
running after you close the app. The app edits the mappings and shows what
the server does.

It's a desktop app built with [mitsuami](https://mitsuami.roboport.com.br), so
it uses each platform's own controls: GTK 4 on Linux, AppKit on macOS and
WinUI 3 on Windows.

## Status

| Platform | Mounting | Reader details |
|---|---|---|
| Linux | Works | Program, command line, user, group, working folder, parents |
| macOS | Not yet, see [macOS](#macos) | Program only |
| Windows | Not yet, see [Windows](#windows) | None |

On Linux I've run it end to end, as my own user. `ls` and `stat` in the
mounted folder never prompted. `cat` blocked until the prompt was answered,
printed the source on Allow, and failed with `Input/output error` on Deny.
With no answer, it failed after the timeout. Escape denied. Writing to a
read-only mapping failed with `Read-only file system`. On a writable one,
`echo >` asked once. A save made by writing a temporary file and renaming it
over the dest asked once and replaced the source's content, and a denied one
left it alone. vim with its default settings loaded the file and saved it.
Files mapped into subfolders showed up there, and the subfolders couldn't
be made or removed. A mount set to `david:wheel` and `0660` listed its
folders as `drwxrwx--- david wheel`, and its files as `-rw-rw----`, or
`-r--r-----` read-only; one set to `root:root` and `0700` gave
`Permission denied` without a prompt. Closing the app left the server and
its mounts up, and a read then got a notification from the server alone
(mako, on Hyprland); clicking it opened the window, whose details could be
selected, and Allow let the read through and took the notification away.
With no answer, the notification went away when time ran out. Reopening the app joined the
running server, and its log screen showed the next answer. Adding and
deleting a mapping restarted the server with the new mappings, and
`readgate stop` stopped it and unmounted everything. Every answer landed in
the log. Mounts for other users haven't been tried yet, because this
machine doesn't have `user_allow_other` on.

## The server

`readgate server` holds the mounts and answers their requests. You don't
start it yourself: opening the app starts it if it isn't running, in a
session of its own, so it keeps running after the app closes, and the next
time the app opens it joins the running server.

- It reads the config once, when it starts. Each change in the app (Add,
  Delete) saves the config and restarts the server, which mounts the new
  set of mappings. Mount Again restarts it too.
- It answers one request at a time. For each one, it starts `readgate
  prompt`, which posts the request's notification and waits, without a
  window, until it's clicked, then shows the window, and reports Allow,
  Deny or the timeout back. A click only reaches the process that posted
  the notification, so the prompt runs until it's answered: mitsuami's
  `App::runs_without_windows` keeps it going with no window open. If it
  can't be shown at all (no display), the request is denied.
- It talks to the app over a Unix socket in `$XDG_RUNTIME_DIR`, one per
  config file: the mappings' status when the app connects, then each answer
  as it's given.
- `readgate stop` stops it. Stopping it unmounts everything. If it's killed
  instead, the mounts unmount by themselves (`auto_unmount`).
- Its output goes to `server.log` in readgate's data folder
  (`~/.local/share/readgate/server.log` on Linux).
- Prompts need a display, so the server gets the one the app had when it
  started the server. It doesn't survive logging out if your session ends
  your processes, as systemd does with `KillUserProcesses`.

## Mount folders

macFUSE and FUSE-T on macOS and WinFsp on Windows only mount folders.
Linux's FUSE can mount a single file, but nothing can be renamed over a
mount point, so editors that save by renaming a temporary file over the
original couldn't save to it. readgate mounts folders on every platform,
so all three behave the same.

Each mapping names its mount folder (`mount`) and where the file shows
inside it (`dest`), which can be in subfolders:

```
/home/shared/gate/        <- readgate's mount
  api-token.txt           <- dest "api-token.txt", gated
  keys/                   <- there because a dest is in it
    deploy_key            <- dest "keys/deploy_key", gated
    .deploy_key.swp       <- an editor's temporary file, not gated
```

- The mount folder is readgate's alone. Mounting hides what's in a folder,
  so readgate refuses to mount over one that isn't empty, and the mappings
  screen says so. A folder that doesn't exist yet gets created.
- Mappings with the same mount folder share one mount. Adding a mapping to
  a mounted folder shows it without mounting again, and deleting the last
  mapping in a folder unmounts it.
- Subfolders exist because a dest is in them. Programs can't make, remove
  or rename folders. Two dests can't clash: the same path twice, or a file
  where another dest needs a folder (`keys` and `keys/deploy_key`), and
  the mappings screen says which one it clashes with.
- Each mount folder has an owner (`user:group`) and a mode, set in its
  `[[mount]]` entry, or yours and `0666` when it has none. The owner is
  the folder's, its subfolders' and its mapped files'. The mode is the
  files', without the write bits for read-only ones. The folders get it
  with execute added wherever there's read, so whoever can read the files
  can get to them: `0660` gives `0660` files (`0440` read-only) in `0770`
  folders, and `0640` gives `0750` folders. Execute in the mode itself is
  ignored for files.
- The kernel checks them (`default_permissions`) before a request gets to
  readgate. Someone they don't let in gets `Permission denied` straight
  away, with no prompt. Someone they do let in still gets the prompt.
- Mapped files can't be deleted, renamed, or moved, and their mode and owner
  can't change.
- Programs can create files in any of the mount's folders when at least one
  of its mappings is writable. An editor's temporary file is the usual one.
  Those files aren't gated, because they belong to the programs that made
  them. Each belongs to whoever created it, as on any filesystem, and
  readgate keeps them at the same path in a scratch folder in its data
  folder (`~/.local/share/readgate/scratch/` on Linux), with the owner in
  an extended attribute, so they're still there after a restart. In a mount whose mappings are all read-only,
  creating files fails with `EROFS`.

## How a read goes

1. A program opens the dest. readgate notes who opened it and lets the open
   succeed, so programs that only look at the file (a file manager, an
   indexer calling `stat` or `open`) never prompt.
2. Its first read on that open file stops in readgate's filesystem. The pid,
   uid and gid come from the kernel, and readgate looks up the rest from the
   pid while the reader is still blocked in `open`, so its pid can't have
   been reused yet.
3. A notification asks: "Allow cat to read notes.txt?", with who's
   asking. Clicking it opens the window with everything about the reader,
   and Deny, Allow for 1 Minute and Allow. Closing the window denies. If a
   second read comes in while one is waiting, it waits in line and its
   reader stays blocked.
4. If nobody answers within 2 minutes, readgate denies the read and logs
   it as timed out. The clock starts when the read comes in, so time spent
   waiting in line counts, and a read whose time ran out in line is denied
   without being shown.
5. Allowed, readgate reads the source and returns the bytes. Denied or
   timed out, the read fails with `EIO`, the error a failing disk gives.
6. The answer holds for that open file until it's closed. Each new open
   asks again, unless an Allow for 1 Minute covers it.

### Allow for 1 Minute

The prompt's Allow for 1 Minute allows the request, and for the next
minute allows the same file, without asking, to the reader's parent
process and everything it starts. A program that runs a command in a shell
for each file it reads (an agent, a build script) gets one prompt, not one
per command.

- When the parent is a shell, the button is a split button, and its menu
  offers the first program up the chain that isn't one instead: "Allow
  claude (pid 51680) for 1 Minute". A program that runs each command in a
  fresh shell (an agent, `make`) starts a new parent each time, so a grant
  to the parent wouldn't reach its next command; one to the program does.
  The shells it goes past are sh, bash, zsh, fish, dash, ksh, mksh, tcsh,
  csh, nu, elvish, xonsh and busybox, by name (for another user's process,
  the name it gives itself). Pid 1 never counts.
- The grant is tied to its process by its pid and its start time, so a later
  process that happens to get the same pid isn't covered.
- It covers the kind of access it was given or less. A grant for reading and
  writing covers both; one for reading doesn't cover writing.
- Only that file. Another mapping, even in the same mount folder, asks.
- Requests waiting in line behind the prompt are checked when their turn
  comes, so those from the same parent go through too.
- The log records them as `granted`. A grant lasts until a minute has
  passed or the server restarts, whichever comes first.
- A reader whose parent readgate can't find gets no such button, and one
  whose parent isn't a shell gets a plain button.

Files are opened with direct I/O, so the kernel never caches their pages.
Without that, the kernel could hand pages you allowed for one reader to the
next one without asking readgate.

### Writable mappings

A mapping is read-only unless its `read_only` is set to false. Opening a
read-only dest for writing fails with `EROFS`, the error for a read-only
filesystem.

A writable dest passes writes through to the source, behind the same
prompt:

- Opening it for writing doesn't ask. The first write asks, with "Allow
  bash to write to notes.txt?" or "read and write" for a file opened for
  both, and the answer holds for that open file like a read's does.
- `open` with `O_TRUNC` (what `>` in a shell does) asks during the open,
  and only empties the source if you allow it. readgate asks the kernel to
  send the truncation along with the open (`FUSE_ATOMIC_O_TRUNC`), so the
  open and its writes share one prompt.
- `truncate` on the path asks by itself.
- Renaming a created file over the dest is how most editors save: they
  write a temporary file next to it, then rename. That rename asks. If you
  allow it, readgate copies the temporary file's content into the source
  and deletes the temporary file. The source keeps its inode, owner and
  mode. If you deny it, the rename fails with `EIO`, and the temporary file
  stays for the program to clean up.
- Setting its times is accepted and ignored, so `touch` works.
- readgate checks that it can write the source when it mounts, so a source
  it can't write shows as not mounted instead of failing after you allow a
  write.

## Sharing with another user (Linux)

FUSE only lets the user who mounted a filesystem use it, unless the mount
has `allow_other`. readgate always mounts with it, and an unprivileged user
can only do that after root enables it once in `/etc/fuse.conf`:

```
user_allow_other
```

Without that line, the mappings screen shows the mapping as not mounted,
with fusermount3's error under the table when you select it.

The mount folder has to satisfy two more conditions:

- You need to be able to mount on it: it has to be yours, or one you can
  write to. readgate creates it when it isn't there, which needs write
  permission on its parent. In a folder that belongs to another user, that
  means a shared folder you can write to, through a group or an ACL.
- The other user needs to be able to reach it, with execute permission on
  every folder on the way, and the mount's owner and mode need to let them
  in. A group they're in, with a mode like `0640` or `0660`, does
  that.

While it's mounted, nobody can delete or rename the mount folder. If readgate
quits or crashes, the folder unmounts (`auto_unmount`) and goes back to being
the empty folder underneath. After you delete a folder's last mapping,
readgate leaves the empty folder behind.

The mount's name in `/proc/mounts` is `readgate`, without the sources'
paths, because every user can read that table.

## The app

**Mappings.** A table of the mappings, with the source, the dest's full
path, whether it's read-only or read and write, its mount folder's owner
and mode, and whether the server mounted it ("…" while the server starts).
Add opens a dialog with the source and the mount folder (both typed, or
picked with the platform's file dialogs), the folder's owner and mode, the
path inside the mount folder, and a Read-only checkbox to untick for a
writable mapping. The mount folder starts as the last one used, with its
owner and mode, and the path as the source's file name. Owner and mode
apply to the whole folder, and left blank they keep what the folder has. A
mode that isn't octal is refused before anything is saved. Delete removes
the selected mappings. Add and Delete restart the server. Mount Again
restarts it too, to retry mappings that failed once you've fixed whatever
stopped them. Selecting a mapping that isn't mounted shows why below the
table.

**Log.** The answers given while the app is open, oldest first, in a read-only monospace text
area. Each answer is a line with the time, answer, program, pid, user and
file, followed by everything known about the reader, indented. The area
follows its end as answers come in, unless you've scrolled up. It starts
empty on each run and keeps the last 1000 answers. Earlier runs' answers are
in the log file.

**The prompt.** A notification first, from the server, whether the app is
open or not: "Allow cat to read notes.txt?", with the program, its pid and
its user. Clicking it opens a window that lists the reader's details,
counts down ("Denies in 1:45"), and offers Deny, Allow for 1 Minute (with
a line saying which process it covers) and Allow. The window is
as tall as what it shows, up to a point, past which the details scroll.
Its text can be selected and copied: a path, a pid. Allow isn't the
default button, so pressing Return doesn't allow. When the countdown
reaches zero, the notification and the window go away and the read is
denied. Closing the window denies too. Where nothing shows notifications,
the window opens straight away.

GNOME Shell drops the notifications of an app without a `.desktop` file for
its id, so on GNOME install one named `br.com.roboport.readgate.desktop`
(in `~/.local/share/applications/`). Other notification servers (mako,
dunst, Plasma's) show them without one.

A notification server that expires notifications takes away what you'd
click: the prompt goes on waiting until its time is up, then denies. mako
expires them after its `default-timeout`; keep readgate's with this in
`~/.config/mako/config`, then `makoctl reload`:

```
[app-name=readgate]
default-timeout=0
```

Trust the Program line, not the Command line. On Linux the program comes
from `/proc/<pid>/exe`, which the kernel keeps. The command line is whatever
the process put in its own `argv`, and a hostile program can write anything
there.

The kernel only lets a user read another user's `exe` and `cwd` links
(root can read everyone's), and readgate runs as you. For another user's
process, the prompt shows what `ps` shows instead: the name and command line
the process gives itself, marked with `*`, and "not visible" for its
working folder. The same goes for each process in "Started by", which also
names its user when it isn't the reader's (`sudo`, `su`). A starred program
can be anything the process says it is.

## Config file

TOML, in the platform's config folder:

| Platform | Path |
|---|---|
| Linux | `~/.config/readgate/config.toml` |
| macOS | `~/Library/Application Support/br.com.roboport.readgate/config.toml` |
| Windows | `%APPDATA%\roboport\readgate\config\config.toml` |

`READGATE_CONFIG=/some/file.toml` uses another file, which helps when trying
things out.

```toml
# Optional. Defaults to access.log in the platform's data folder
# (~/.local/share/readgate/access.log on Linux).
log_file = "/home/me/readgate-access.log"

# Optional. Seconds a prompt waits before it denies. Defaults to 120.
prompt_timeout_secs = 120

# Optional, per mount folder. Without one, its files are yours and 0666
# (and its folders 0777).
[[mount]]
path = "/home/shared/gate"
# user:group, user (with that user's group) or :group, by name or number.
owner = "me:shared"
# The files' mode. Folders get execute where there's read: 0770 here.
mode = "0660"

[[mapping]]
id = 1
source = "/home/me/secrets/api-token.txt"
mount = "/home/shared/gate"
dest = "api-token.txt"
# Optional. Defaults to true.
read_only = true

[[mapping]]
id = 2
source = "/home/me/.ssh/deploy_key"
mount = "/home/shared/gate"
dest = "keys/deploy_key"

[[mapping]]
id = 3
source = "/home/me/notes.txt"
mount = "/home/shared/gate"
dest = "notes.txt"
read_only = false
```

`dest` is relative to `mount`, and can't leave it (`..` and absolute paths
are refused). A mapping from an earlier version has only `dest`, as a full
path. readgate reads it as that path's folder for `mount` and its file name
for `dest`, and writes it that way the next time it saves the config.

readgate reads the file at start and writes it back after every Add and
Delete. It writes to a temporary file and renames it over the old one, so a
crash never leaves half a config. You can edit the file by hand while the app
is closed. Edits made while it runs get overwritten on the next change. Ids
only need to be unique. The log uses them to tell mappings apart.

## Access log

One JSON object per line, appended for each answer:

```json
{"time":"2026-11-06T10:05:33.042806855Z","mapping_id":1,"source":"/home/me/secrets/api-token.txt","dest":"/home/shared/gate/api-token.txt","reader":{"pid":973497,"uid":1001,"gid":1001,"user":"guest","exe":"/usr/bin/cat","cmdline":["cat","/home/shared/gate/api-token.txt"],"cwd":"/home/guest","ancestors":[{"pid":973495,"exe":"/usr/bin/bash"}]},"access":"read","decision":"allowed"}
```

`dest` is the dest's full path. `access` is `read`, `write` or
`read_write`, from how the file was opened.
A save by rename is a `write`. `decision` is `allowed`, `denied`,
`timed_out` or `granted` (allowed by an earlier Allow for 1 Minute, without
asking). Times are UTC in RFC 3339. The log screen shows them in local
time.

readgate only ever appends to the file. Each run adds its answers after the
earlier runs' answers, and nothing in readgate reads the file back. Each line
goes out in a single append, so you can follow the file with `tail -f` or
read it with `jq` while readgate runs.

## Building

mitsuami 2.2.0 isn't on crates.io yet. Until it is, `Cargo.toml` builds it
from a checkout at `../mitsuami`, through `[patch.crates-io]`. Once it's
released, delete that section. readgate needs `App::runs_without_windows`
and `quit`, which are in the local checkout's 2.2.0 and not in 2.1.

readgate is distributed as a binary, so every dependency is pinned to an
exact version.

Linux needs Rust 1.95 or newer, GTK 4.10+ and libadwaita 1.4+ development
files, and `fusermount3`, which comes with the fuse3 package. libfuse itself
isn't linked: fuser talks to the kernel and mounts through fusermount3.

```sh
cargo run
READGATE_CONFIG=/tmp/try.toml cargo run   # with a throwaway config
```

To try it without dialogs landing on your desktop, run it on a private GTK
Broadway display and open http://localhost:8087 in a browser:

```sh
gtk4-broadwayd :7 &
GDK_BACKEND=broadway BROADWAY_DISPLAY=:7 cargo run
```

## Code

| File | |
|---|---|
| `src/main.rs` | The commands: the app, `server`, `stop`, `prompt` |
| `src/config.rs` | Config file, mappings, mount folders' settings, the scratch folders |
| `src/server.rs` | The server: mounts, a prompt window per request, the log, the app's connections |
| `src/ipc.rs` | The app and the server's socket: starting, stopping, following the server |
| `src/perms.rs` | A mount folder's owner and mode, from text to numbers |
| `src/gate.rs` | A filesystem thread sends a request and blocks; the server answers |
| `src/mount/mod.rs` | Groups mappings by mount folder, checks for clashes, mounts each folder |
| `src/mount/fuse.rs` | The FUSE filesystem: a tree of gated files, their folders, and created files |
| `src/reader.rs` | What the platform says about a pid |
| `src/access_log.rs` | Log entries, and appending them to the file |
| `src/ui/` | The app's window, its mappings and log screens, and the prompt (notification and window) |

FUSE requests come in on fuser's session thread. A request that needs an
answer moves to a thread of its own, so a prompt left up for minutes doesn't
stop other requests, such as another reader's `getattr` or a `release`. That
thread sends the request to the server's loop over a channel and blocks on
the reply. If the loop is gone, the answer is Deny. The prompt window ends
itself at the deadline, and the server stops one that runs 2 seconds over.
As a backstop, the filesystem thread itself stops waiting 5 seconds after
the deadline, so a stuck server can't hold a reader forever.

## Limits

- readgate's server has to be running. While it's not, the mount folder is
  empty.
- Root can read the source directly. readgate controls one path to a file,
  not who can reach the file.
- An allowed reader can do what it likes with the bytes it got, including
  passing its open file to another process. That other process reads on the
  same answer.
- If the reader is interrupted while it waits (Ctrl-C), the prompt stays up
  until you answer or it times out, and that answer goes nowhere. It's still
  logged.
- Linux kernels set no FUSE request timeout unless one is configured
  (`fs.fuse.default_request_timeout`, Linux 6.14 and later). If yours sets
  one shorter than `prompt_timeout_secs`, the kernel fails the request first.
- On Linux, the details come from `/proc`, so with `hidepid` on, readgate
  only sees the reader's pid and ids.
- A save by rename copies into the source, so it isn't atomic: a crash in
  the middle leaves the source half written. The rename itself is atomic
  for the program, which only sees it succeed once the copy is done.
- Files programs create in a mount with a writable mapping take space in
  your data folder, and any user who can reach the mount can create them.

## Plans

### macOS

The same folder design, through macFUSE or FUSE-T. fuser builds for macOS,
so `src/mount/fuse.rs` should mostly carry over. macFUSE stops waiting on a
filesystem after its `daemon_timeout`, 60 seconds by default, so readgate
will mount with a long one. Reader details need `proc_pidinfo` and
`KERN_PROCARGS2` for the command line, working folder and parents.
`src/reader.rs` only gets the program now.

### Windows

The same folder design, through [WinFsp](https://winfsp.dev) and the
`winfsp` crate. WinFsp mounts a drive letter or a folder, and creates the
folder itself, so it must not exist yet. readgate will remove an empty
mount folder before mounting. Reader details come from the request's process id:
`QueryFullProcessImageNameW`, the process's command line, and its token's
user SID. The crate isn't a dependency yet, because building it needs the
WinFsp SDK.

### Later

- Remember an answer: "always allow this program" for a mapping, kept in
  the config.
- A notification when the window is hidden, and running from the tray or
  menu bar instead of keeping a window open.
- Tests with `mitsuami-test` for the screens and the prompt, using its
  scripted dialogs.
- Starting the server at login (a systemd user service, a macOS launch
  agent), instead of when the app opens.
- Changing a mount folder's owner and mode for a folder that's already set,
  from the mappings screen instead of through Add.

## License

MIT or Apache-2.0, at your option.
