# Running chriscord as a systemd service

With the service installed, the server starts at boot, restarts itself if
it ever crashes, and is controlled with `systemctl`:

| Do this | Command |
|---|---|
| Restart (e.g. after updating) | `sudo systemctl restart chriscord` |
| Stop / start | `sudo systemctl stop chriscord` · `sudo systemctl start chriscord` |
| Is it running? | `systemctl status chriscord` |
| Follow the log | `journalctl -u chriscord -f` |
| Don't start at boot | `sudo systemctl disable chriscord` |
| **Show the owner key** | `sudo -u chriscord chriscord-server --data-dir /var/lib/chriscord owner-key` |
| Make a new owner key | `sudo -u chriscord chriscord-server --data-dir /var/lib/chriscord reset-owner-key` then restart |

When the server stops or restarts, everyone's app shows that it's
restarting, reconnects on its own when it's back, and anyone who was in a
call rejoins it.

## Installing

```sh
cargo build --release
sudo deploy/install.sh /path/to/where/you/run/it/now   # moves your existing data
# or, for a brand-new server:
sudo deploy/install.sh
```

The path is the directory you've been running the server from — the one
with `chriscord.db`, `uploads/`, `pfps/` and `server_assets/` in it. Those
are **copied** to `/var/lib/chriscord`; the originals stay where they are
until you delete them. Stop the old (non-service) server before installing.
The install prints the owner key at the end.

## Updating

```sh
git pull
cargo build --release
sudo deploy/install.sh
```

Running the installer again replaces the program and restarts the service;
your data and settings are kept.

## Where things are

| What | Where |
|---|---|
| Program | `/usr/local/bin/chriscord-server` |
| Data (database, uploads, pictures, server images) | `/var/lib/chriscord` |
| Optional settings (e.g. `CHRISCORD_PUBLIC_IP`) | `/etc/chriscord/chriscord.env` |
| The service | `/etc/systemd/system/chriscord.service` |

The service runs as its own `chriscord` user, which can only write to
`/var/lib/chriscord`. Run `owner-key` as that user (as shown above), not as
plain root — the server will remind you if you try.

## The owner key

It's no longer printed in the log when running as a service (logs can be
read by anyone in the `adm`/`systemd-journal` groups). Get it with the
`owner-key` command above whenever you need it. Running the server by hand
in a terminal still shows it in the start-up banner, as before.

## Backups

Back up `/var/lib/chriscord`. For the database, while the server is running:

```sh
sudo -u chriscord sqlite3 /var/lib/chriscord/chriscord.db ".backup '/var/lib/chriscord/backup.db'"
```

## More servers on the same machine

The admin panel's **Hosted servers** page runs other Chriscord servers from this
one — each completely separate (its own members, channels, owner key and admin
panel), each on its own port. Pick a name and a port, and it's created and
started; it starts and stops with this service from then on, and is restarted if
it ever stops by itself.

* Forward each hosted server's port as **TCP + UDP**, like 7070 (chat and voice
  share it), or point a domain at it with your reverse proxy. People add it in
  the app as `your-address:port`.
* Its data lives in `/var/lib/chriscord/servers/<id>/` (shown in the panel) — so
  backing up `/var/lib/chriscord` covers them too.
* *Owner key* copies its owner key; *Admin panel* opens its own panel.
* *Remove* stops it and takes it off the list, keeping its data unless you tick
  *Delete its data too*.

You can also run a separate server by hand with `--port`:
`chriscord-server --data-dir /some/dir --port 7071`.

## Importing a Discord server

`chriscord-import` (its own folder/repo) brings DiscordChatExporter HTML exports
in, end-to-end encrypted. The people it imports show up in the admin panel under
**Imported members**, where you approve members' claims ("This is me" in the app)
or link them yourself. See chriscord-import's README.
