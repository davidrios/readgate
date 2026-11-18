# Status

| Platform | Mounting | Reader details |
|---|---|---|
| Linux | Works | Program, command line, user, group, working folder, parents |
| macOS | Not yet, see [macOS](README.md#macos) | Program only |
| Windows | Not yet, see [Windows](README.md#windows) | None |

## Linux

Everything in the README has been run end to end on Linux.

`ls` and `stat` in the mounted folder never prompted. `cat` blocked until
the prompt was answered, printed the source on Allow, and failed with
`Input/output error` on Deny. With no answer, it failed after the timeout.
Escape denied. Writing to a read-only mapping failed with `Read-only file
system`. On a writable one, `echo >` asked once. A save made by writing a
temporary file and renaming it over the dest asked once and replaced the
source's content, and a denied one left it alone. vim with its default
settings loaded the file and saved it. Files mapped into subfolders showed
up there, and the subfolders couldn't be made or removed. A mount set to
`david:wheel` and `0660` listed its folders as `drwxrwx--- david wheel`,
and its files as `-rw-rw----`, or `-r--r-----` read-only; one set to
`root:root` and `0700` gave `Permission denied` without a prompt. Another
user's programs (an agent running as its own user, with `user_allow_other`
on) read through the mount, each read behind a prompt, and Allow for 1
Minute covered the agent's later commands. Every answer landed in the log.

Running in the tray: closing the window left readgate and its mounts up,
and a read then got a notification; clicking it, or the question in the
tray's menu, opened the prompt. Launching readgate again opened the running
one's window. Quit in the tray's menu and `readgate stop` unmounted
everything and ended it, Mount Again mounted again each time, and starting
over a dead mount left by an earlier version cleared it and mounted.

Folder mappings: a source folder's files listed in their dest, without its
subfolders or symlinks; files added to or removed from the source showed
up within a second; a read asked, per file, and was logged with the folder
mapping's id and that file's paths; on a writable one, a write and a save
by rename each asked; a folder mapping at the mount folder itself kept
another mapping out of that mount, with the clash as the reason.
