# Setting up the Velm board server

This guide sets up one machine — your server — to hold your boards and make them reachable
from any browser you open. When it is done you go to one web address on an iPad, a Windows
desktop, a borrowed laptop, anything with a modern browser, and your boards are there.

**Every step is numbered.** Each is marked either:

- **YOU** — you type it or click it, and
- **AUTOMATIC** — a command does the work and tells you whether it worked.

**Nothing in this guide deletes anything.** The boards on your Mac are copied, never moved,
and the Mac's copy stays exactly as it is afterwards as a permanent fallback.

---

## How the pieces fit

```
   the app                              your server
   https://velm.<your-app-domain>       https://boards.<your-domain>
   ┌───────────────────────┐            ┌──────────────────────────────┐
   │  velm.wasm            │            │  velmd                       │
   │  velm.js              │  ───────►  │  your boards (SQLite)        │
   │  index.html           │   HTTPS    │  your images (blob store)    │
   │  holds no data, ever  │            │  only you can sign in        │
   └───────────────────────┘            └──────────────────────────────┘
         ▲          ▲          ▲
      iPad      Windows     any computer
```

The app is just a program. Your boards never touch it — they go straight from your server to
whichever browser you are sitting in front of.

---

## Before you start — five things we need from you

Tell us these and the rest of this guide gets more specific. Guessing wrong here wastes a day.

1. **What is the server?** Run this on it and send us the line it prints:
   ```
   uname -srm
   ```
   Also: how much RAM, and roughly how much free disk.
2. **Is there a second, separate disk?** Backups need one. The same disk is not a backup — a
   single drive failure would take the boards and the backup together.
3. **Is it always on, and where does it live** — your home, a rack, a rented VPS?
4. **What iPadOS version is the iPad on?** It must be **iPadOS 26 or newer.** Velm draws with
   WebGPU, and Safari only got WebGPU in iPadOS 26. Check under
   Settings ▸ General ▸ About ▸ Software Version. On an older iPad this cannot work, and no
   setting changes that.
5. **Do you own a domain name?** You need one, or a free subdomain from a dynamic-DNS
   provider. A bare IP address will not do — see the box below.

### ⚠️ Why a plain IP address will not work

Browsers refuse to hand a web page the graphics hardware unless the page arrived over a
proper secure address — `https://` with a real certificate. A page served from something like
`http://192.168.1.50:8787` will load, and then draw **nothing at all**, on every browser. It
looks exactly like a broken app and it is not; it is the browser declining.

There is a second reason too: browsers now block pages on the public internet from talking to
machines on a private home network. So the server needs a real name and a real certificate.
Both are free. Part 4 does it.

### What this guide assumes

- The server runs **Ubuntu 24.04 or newer**, or **Debian 12 or newer**.
- You can reach it with `ssh`, and you can use `sudo` on it.
- You own a domain, or can get a free subdomain.

If your server is a NAS, a Mac or a Windows box, tell us — the shape is identical but steps
12 and 24 (starting things automatically) are different.

---

## Part 1 — Prepare the server

**1. YOU** — sign in to the server:

```
ssh <you>@<your-server>
```

**2. YOU** — install what is needed to build:

```
sudo apt update
sudo apt install -y build-essential curl git pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

**3. YOU** — make the folders. `data` is where boards will live, `incoming` is a landing area
for the copy from your Mac, `backup` is for later:

```
sudo mkdir -p /srv/velm/data /srv/velm/incoming /srv/velm/backup
sudo chown -R "$USER" /srv/velm
```

**4. YOU** — get the source and build the server program:

```
git clone <the Velm repository URL> ~/velm
cd ~/velm
cargo build --release -p velmd
```

**AUTOMATIC** — the first build takes 5–15 minutes. Check it worked:

```
~/velm/target/release/velmd --version
```

---

## Part 2 — Copy your boards across

**Read this whole part before starting it.** This is the part that matters, because these
boards cannot be re-made.

**5. YOU — on the Mac.** Quit Velm from its menu (Velm ▸ Quit, or ⌘Q).
**Do not Force Quit.** Quitting normally makes Velm finish writing every board that is open.
Force-quitting skips that.

**6. YOU — on the Mac.** Check nothing was left half-written:

```
ls ~/Library/Application\ Support/Vellum/boards/*.vellum-wal
```

You want **"No such file or directory."** If you see filenames instead, open Velm again and
quit it properly, then check again. If they are still there, carry on — the next steps copy
them too, so nothing is lost either way.

**7. AUTOMATIC — on the Mac.** Take a fingerprint of every file, so we can prove later that
the copy arrived intact. This only reads; it does not open a single board:

```
~/velm/target/release/velmd manifest \
  --data "$HOME/Library/Application Support/Vellum" \
  --out "$HOME/Desktop/velm-manifest.json"
```

It prints something like `58 boards, 1,842 files, fingerprinted`.

**8. YOU — on the Mac.** Copy everything to the server:

```
rsync -av --checksum \
  "$HOME/Library/Application Support/Vellum/" \
  <you>@<your-server>:/srv/velm/incoming/

scp "$HOME/Desktop/velm-manifest.json" <you>@<your-server>:/srv/velm/
```

> ⚠️ **Never add `--delete` to that command.** Not now, not later, not in a script somebody
> writes next year. It is the single option that could turn this copy into a deletion, and
> there is no reason to ever use it here.

This takes a while — the images are most of it. It is safe to interrupt with Ctrl-C and run
again; it picks up where it left off.

**9. AUTOMATIC — on the server.** Prove the copy is identical, byte for byte:

```
~/velm/target/release/velmd verify \
  --data /srv/velm/incoming \
  --manifest /srv/velm/velm-manifest.json
```

You want: `58 boards · 1,842 files · 0 mismatches · 0 missing`.

> **If any number is not zero, stop here and tell us.** Do not continue. Nothing is broken
> yet at this point, and continuing is how it would become broken.

**10. AUTOMATIC — on the server.** Put the boards into place. This is another copy, so
`incoming` stays exactly as it is:

```
~/velm/target/release/velmd import \
  --from /srv/velm/incoming \
  --data /srv/velm/data
```

It opens each board, checks it, and prints a table of every board with its name and how many
things are on it. **Compare a few names against what you see in Velm on the Mac.**

**11. YOU.** Leave the Mac alone. Do not delete its boards. Do not delete
`/srv/velm/incoming`. Disk is cheap and these boards are not replaceable.

---

## Part 3 — Run the server

**12. YOU** — install it as a service, so it starts on boot and restarts if it stops.
Replace `<you>` with your username on the server:

```
sudo tee /etc/systemd/system/velmd.service >/dev/null <<'EOF'
[Unit]
Description=Velm board server
After=network-online.target

[Service]
ExecStart=/home/<you>/velm/target/release/velmd serve \
  --data /srv/velm/data --bind 127.0.0.1:8787
User=<you>
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now velmd
```

**13. AUTOMATIC** — check it is alive:

```
systemctl status velmd
curl -s http://127.0.0.1:8787/api/v1/health
```

You should get a line of JSON with a version and an uptime.

`--bind 127.0.0.1` means it currently answers **only on the server itself**. That is
deliberate. Part 4 is how it becomes reachable, with a certificate and a login in front.

**14. YOU** — set the passphrase you will type to sign in. Choose something you can type on a
touch keyboard but nobody would guess:

```
~/velm/target/release/velmd set-passphrase --data /srv/velm/data
sudo systemctl restart velmd
```

> ⚠️ **Do not skip this step and do not go to Part 4 without it.** After Part 4 the server is
> reachable from the internet, and the passphrase is what stands between a stranger and every
> board you own.

---

## Part 4 — Put it on the internet, safely

**15. YOU** — point a domain at the server. In your domain registrar's DNS settings, add an
**A record** for something like `boards.<your-domain>` pointing at the server's public IP
address. If the server is at home, you will also need to forward **port 443 only** to it on
your router. Do not forward port 8787 or port 22.

**16. YOU** — install Caddy. It gets a real certificate from Let's Encrypt automatically and
renews it forever, with no further work from you:

```
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt update && sudo apt install -y caddy
```

**17. YOU** — tell Caddy about Velm. Replace both placeholder names:

```
sudo tee /etc/caddy/Caddyfile >/dev/null <<'EOF'
boards.<your-domain> {
    reverse_proxy 127.0.0.1:8787
}
EOF

sudo systemctl reload caddy
```

**AUTOMATIC** — Caddy fetches a certificate within about a minute. Check from any machine:

```
curl -s https://boards.<your-domain>/api/v1/health
```

Seeing JSON here, over `https://`, means the hard part is done.

**18. YOU** — tell velmd which app site is allowed to talk to it. Edit the service file from
step 12 and add this to the `ExecStart` line:

```
--app-origin https://velm.<your-app-domain>
```

Then:

```
sudo systemctl daemon-reload && sudo systemctl restart velmd
```

Without this, the browser will refuse to let the app read your boards. This is the browser
protecting you, not a fault.

**19. YOU** — open the app site on any computer, enter `https://boards.<your-domain>` as your
server address, and sign in with the passphrase from step 14. Your boards should be listed.

**20. YOU** — do the same on the iPad, in Safari.

**21. YOU — the test that proves it.** Open the same board on both. Move something on the
iPad. Within about a second it moves on the other computer too.

### Part 4b — Hardening, now that it is reachable from anywhere

**22. YOU** — turn on automatic security updates:

```
sudo apt install -y unattended-upgrades
sudo dpkg-reconfigure --priority=low unattended-upgrades
```

**23. YOU** — close everything except what is needed:

```
sudo ufw allow 22/tcp
sudo ufw allow 443/tcp
sudo ufw --force enable
```

velmd throttles failed sign-in attempts by itself, and it has **no delete route at all** —
there is no request anyone can send it, signed in or not, that removes a board file.

---

## Part 5 — Backups

Your boards are now in two places: the Mac (frozen, untouched) and the server (live,
changing). Only the server changes, so only the server needs backing up — but it needs it
properly, because it now holds your newest work.

**24. YOU** — attach a **second** disk and mount it.

**25. YOU** — turn on a nightly backup. Replace `<you>` and the disk path:

```
sudo tee /etc/systemd/system/velmd-backup.service >/dev/null <<'EOF'
[Unit]
Description=Velm nightly backup

[Service]
Type=oneshot
ExecStart=/home/<you>/velm/target/release/velmd backup \
  --data /srv/velm/data --to /mnt/<your-backup-disk>/velm --verify
User=<you>
EOF

sudo tee /etc/systemd/system/velmd-backup.timer >/dev/null <<'EOF'
[Unit]
Description=Velm nightly backup

[Timer]
OnCalendar=*-*-* 03:30:00
Persistent=true

[Install]
WantedBy=timers.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now velmd-backup.timer
```

**26. AUTOMATIC** — run one now, by hand, and read what it says:

```
sudo systemctl start velmd-backup.service
journalctl -u velmd-backup.service -n 40
```

It reopens every backed-up board and compares it against the live one. You want `0 mismatches`.

**27. YOU — the drill. Do this once, now.** A backup nobody has ever restored is a guess, not
a backup. Restore last night's copy into a scratch folder and open a board from it:

```
~/velm/target/release/velmd serve \
  --data /mnt/<your-backup-disk>/velm/<latest-date> \
  --bind 127.0.0.1:8788 --read-only
```

Point a browser at it and open a board. If it opens and looks right, your backups work. Stop
that second server with Ctrl-C afterwards.

**28. YOU — offsite, when you are ready.** A fire or a burglary takes both disks in the same
minute. An encrypted weekly copy to cheap online storage covers that. Ask and we will write
that step for whichever provider you pick.

---

## The short list to remember

- **The Mac's boards are never touched by any of this.** Keep them. Forever.
- **Never add `--delete` to an rsync involving your boards.**
- **Never run `velmd` on the Mac pointed at the Mac's own boards folder** while Velm is open.
  Two programs writing one board file at the same time is the one thing that genuinely
  corrupts one.
- **Never expose the server without setting the passphrase** (step 14).
- If something looks wrong, **stop and ask before tidying anything up.** Nothing here is
  urgent enough to risk a board over.

## Where things live on the server

```
/srv/velm/data/boards/       your boards, one file each
/srv/velm/data/blobs/        your images, stored once each by content
/srv/velm/data/runtime/      the server's lock file and sign-in token
/srv/velm/incoming/          the untouched copy that came from the Mac
/mnt/<your-backup-disk>/     nightly backups
```
