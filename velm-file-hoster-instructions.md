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
                       your server
                       https://boards.<your-domain>
                       ┌──────────────────────────────────┐
                       │  velmd                           │
                       │    the Velm app  (velm.wasm)     │
        ──── HTTPS ──► │    your boards   (SQLite)        │
                       │    your images   (blob store)    │
                       │  one token guards the last two   │
                       └──────────────────────────────────┘
    ▲          ▲          ▲
  iPad      Windows     any computer
```

**One address, serving both the app and the boards** — that is the whole hosting decision,
and it is worth understanding because it removes three separate problems at once. When the
page and the data come from the same place, the browser never treats one as reaching across
to the other: no CORS to configure, no mixed-content block, and none of Chrome's new rules
about public pages talking to private machines.

It also means there is nothing to trust but your own server. The app is not hosted by anyone
else; your boards are not copied anywhere else.

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
12 and 25 (starting things automatically) are different.

---

## Part 1 — Prepare the server

**1. YOU** — sign in to the server:

```
ssh <you>@<your-server>
```

**2. YOU** — install what is needed to build:

```
sudo apt update
sudo apt install -y build-essential curl git pkg-config openssl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
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

You want either **"no matches found"** (which is what Terminal's own shell says) or **"No such
file or directory."** Both mean the same thing: there are none, and Velm shut down cleanly. If
you see filenames instead, open Velm again and quit it from the menu, then check again — and if
they are still there, carry on anyway, because the next steps copy them too and nothing is
lost either way. If you see filenames instead, open Velm again and
quit it properly, then check again. If they are still there, carry on — the next steps copy
them too, so nothing is lost either way.

**7. AUTOMATIC — on the Mac.** Take a fingerprint of every file, so we can prove later that
the copy arrived intact. This only reads; it does not open a single board:

> **First, build the tool on the Mac.** `~/velm` was created *on the server* in step 4; the
> Mac has nothing yet, and a Linux binary could not be copied back anyway. From your Velm
> checkout on the Mac:
>
> ```
> cargo build --release -p velmd
> ```
>
> Every command below that says `velmd` on the Mac means `./target/release/velmd`, run from
> that checkout.

```
./target/release/velmd manifest \
  --data "$HOME/Library/Application Support/Vellum" \
  --out "$HOME/Desktop/velm-manifest.json"
```

It prints something like `45 boards, 3519 files, 1.5 GB, fingerprinted`. **The counts are
whatever you actually have** — they are for comparing against the next step, not a target to
hit.

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

You want the **last two numbers to be zero**: `… · 0 mismatches · 0 missing`. The board and
file counts will be whatever step 7 printed; only mismatches and missing files matter here.

> If the only mismatch named is `.DS_Store`, that is a Finder bookkeeping file rather than
> board content — Finder rewrites it whenever the folder's view changes. Re-run the manifest
> and the check and it will settle. **A mismatch on anything under `boards/` or `blobs/` is
> real: stop.**

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

**12. YOU** — build the browser client, make the access token, and install the server as a
service so it starts on boot.
Replace `<you>` with your username on the server:

First build the browser client, once — the server serves it, but it is not part of the
server binary:

```
cd ~/velm
cargo install wasm-bindgen-cli --version "$(awk '/^name = "wasm-bindgen"$/{getline; gsub(/[",]/,""); print $3; exit}' Cargo.lock)"
rustup target add wasm32-unknown-unknown
./scripts/build-web.sh
```

Then make the secret that stands between a stranger and every board you own. It is a long
random string rather than a passphrase you invent, because you will never type it — the token
lives in the bookmark you save in step 19, and opening the address without it shows an empty
list rather than your boards:

```
mkdir -p /srv/velm/secret && chmod 700 /srv/velm/secret
openssl rand -hex 32 > /srv/velm/secret/token
chmod 600 /srv/velm/secret/token
cat /srv/velm/secret/token          # copy this; you need it in step 19
```

Now install the service. Replace `<you>` with your username on the server:

```
sudo tee /etc/systemd/system/velmd.service >/dev/null <<'EOF'
[Unit]
Description=Velm board server
Wants=network-online.target
After=network-online.target

[Service]
# The token is read from a file rather than written here, so it is not in the unit,
# not in `systemctl show`, and not in `ps`.
EnvironmentFile=/srv/velm/secret/velmd.env
ExecStart=/home/<you>/velm/target/release/velmd serve \
  --data /srv/velm/data/boards \
  --blobs /srv/velm/data/blobs \
  --web /home/<you>/velm/web/dist \
  --addr 127.0.0.1:8787
User=<you>
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=/srv/velm

[Install]
WantedBy=multi-user.target
EOF

printf 'VELMD_TOKEN=%s\n' "$(cat /srv/velm/secret/token)" | sudo tee /srv/velm/secret/velmd.env >/dev/null
sudo chmod 600 /srv/velm/secret/velmd.env
sudo chown <you> /srv/velm/secret/velmd.env

sudo systemctl daemon-reload
sudo systemctl enable --now velmd
```

**13. AUTOMATIC** — check it is alive:

```
systemctl status velmd
curl -s http://127.0.0.1:8787/api/v1/health
curl -s -H "Authorization: Bearer $(cat /srv/velm/secret/token)" \
     http://127.0.0.1:8787/api/v1/boards
```

The first answers `{"velmd":"…","ok":true}` — health is deliberately outside the token, so
"the server is not running" and "my token is wrong" are two different answers rather than one.
The second lists your boards. Without the token it answers `401`, which is the point.

`--addr 127.0.0.1` means it currently answers **only on the server itself**. That is
deliberate. Part 4 is how it becomes reachable, with a certificate in front.

**14. AUTOMATIC** — the server will not let you skip the secret. Bound to anything but
`127.0.0.1` with no `VELMD_TOKEN` set, it refuses to start and says so, **before the socket is
open** — so there is no window, however short, in which your boards are on the internet with
no gate at all. Try it if you like:

```
~/velm/target/release/velmd serve --data /srv/velm/data/boards --blobs /srv/velm/data/blobs --addr 0.0.0.0:8787
```

> ⚠️ **The token guards the boards, not the app.** Anyone who reaches your address can load
> the Velm client itself, which is the same open-source code as the public repository. What
> they cannot get without the token is a board, a board's name, a picture, or even the fact
> that you have any boards. That split is not a compromise: a web page cannot attach a header
> to its own script and wasm requests, so a client behind the gate could not load itself.

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

**17. YOU** — tell Caddy about Velm. Replace `boards.<your-domain>` with the name you
set up in step 15:

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

**18. AUTOMATIC — there is nothing to configure here, and that is the point.** velmd serves
the Velm app *and* your boards from the same address, so the browser never treats one as
talking to the other. That single decision is what makes CORS, mixed content and Chrome's
Private Network Access rules all stop applying at once. There is an `--app-origin` flag for
the day you want to host the app somewhere else; you do not need it.

**19. YOU** — open `https://boards.<your-domain>/?token=<the token from step 12>` on any
computer. You will see **Your boards**. Click one.

The list is every board *file* on the server, so it includes anything sitting in the desktop
app's **Recently deleted** — nothing is ever removed from disk, which is deliberate. Empty
Recently deleted on the Mac first if you would rather not see them, then re-run Part 2.

Then bookmark that link. There is no login form and no session to expire: the link *is* the
key, and the browser remembers it. Anyone you send that link to can read your boards, so send
it the way you would send a house key.

**20. YOU** — open the same link on the iPad, in Safari. **iPadOS must be 26 or newer** —
Safari only gained WebGPU in 26, and on anything older the page loads and the board does not
draw. One finger pans, two fingers pinch to zoom.

> To check the touch handling on your own device rather than taking this document's word for
> it, add `&selftest=touch` to the link. It drives five gestures through the page's own event
> handlers and prints a PASS or FAIL line with the numbers it measured.

**21. AUTOMATIC — what you will and will not be able to do.** The browser client is a
**reader**. You can open any board from any computer, pan, zoom, and see your stickies, frames,
pictures, pen strokes and connectors. You **cannot yet edit a board in a browser** — the
desktop app is still the only place a board changes, and until two-way syncing is built, a
board edited on the Mac has to be copied across again (Part 2) for the server to see the
change.

That is deliberate rather than unfinished. A browser tab can be killed by the operating system
with no warning and no chance to save; a client that could edit would, at that moment, be
holding the only recent copy of a board that cannot be re-imported.

### ⚠️ The one mistake that would undo all of this

When you put the reverse proxy in front (step 25), velmd ends up listening on
`127.0.0.1` — the proxy talks to it, nothing else does. That is correct and it is what
the steps above tell you to do.

**It is also the one arrangement where forgetting `VELMD_TOKEN` is invisible.** velmd
refuses to start on a public address without a token, and it says so loudly. It does
*not* refuse to start on `127.0.0.1` without one, because that is how you run it on your
own laptop while testing. So a server behind a proxy with no token set starts perfectly,
says nothing is wrong, and serves every board you own to anyone who finds the address.

velmd now catches this itself: a request that arrives through a proxy, on a server with
no token, is refused with a message naming the problem. **You should never see it.** If
you do, it means `VELMD_TOKEN` is not reaching the service — check step 12 and
`systemctl show velmd -p Environment`.

There is no version of this where a missing token is safe once the proxy is in front.

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

velmd has **no delete route at all** — there is no request anyone can send it, with or without
the token, that removes or changes a board file. Every route that returns anything is a `GET` — it also
answers a browser's `OPTIONS` preflight with an empty 204 — and a test in its own source tree
fails the build if code that could unlink or move a file is ever added to it.

It does **not** rate-limit wrong tokens, and it does not need to: the token is 32 random bytes
and it is compared in constant time, so there is nothing to guess at and nothing to learn from
how long a wrong guess takes. If you would rather have a limiter anyway, Caddy can do it in
front — but do not tell yourself you have one when you have not.

---

## Part 5 — Backups

Your boards are now in two places: the Mac (frozen, untouched) and the server (live,
changing). Only the server changes, so only the server needs backing up — but it needs it
properly, because it now holds your newest work.

**24. YOU** — attach a **second** disk and mount it.

**25. YOU** — turn on a nightly backup. Replace `<you>` and the disk path:

```
sudo tee /usr/local/bin/velm-backup >/dev/null <<'EOF'
#!/bin/bash
# Copy, then prove the copy. Never --delete: one flag is the only way this could
# destroy something, and there is no version of "tidying up" worth that risk.
set -euo pipefail
NIGHT=/mnt/<your-backup-disk>/velm/$(date +%F)
mkdir -p "$NIGHT"
# ⚠ velmd is stopped for the copy, and that is not politeness — it opens boards with SQLite,
# which writes a -wal sidecar, so a file that changes between the copy and the check below
# reports as a mismatch. A backup that cries wolf teaches you to ignore backup failures,
# which is worse than having none.
sudo systemctl stop velmd
trap 'sudo systemctl start velmd' EXIT
rsync -a /srv/velm/data/ "$NIGHT/"
/home/<you>/velm/target/release/velmd manifest --data /srv/velm/data --out /tmp/velm-live.json
/home/<you>/velm/target/release/velmd verify   --data "$NIGHT" --manifest /tmp/velm-live.json
EOF
sudo chmod +x /usr/local/bin/velm-backup

sudo tee /etc/systemd/system/velmd-backup.service >/dev/null <<'EOF'
[Unit]
Description=Velm nightly backup

[Service]
Type=oneshot
ExecStart=/usr/local/bin/velm-backup
# root, because the script stops and starts velmd around the copy.
User=root
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

`velmd verify` recomputes the BLAKE3 hash of every file in the backup and compares it against
a manifest taken from the live directory a moment earlier. You want **`0 mismatches · 0
missing`**. Anything else: stop and ask, before touching anything.

**27. YOU — the drill. Do this once, now.** A backup nobody has ever restored is a guess, not
a backup. Restore last night's copy into a scratch folder and open a board from it:

```
VELMD_TOKEN=$(cat /srv/velm/secret/token) ~/velm/target/release/velmd serve \
  --data /mnt/<your-backup-disk>/velm/<latest-date>/boards \
  --blobs /mnt/<your-backup-disk>/velm/<latest-date>/blobs \
  --web ~/velm/web/dist \
  --addr 127.0.0.1:8788
```

Then, **from your own machine**, open a tunnel to the server and point a browser at it. Use
whatever address you normally `ssh` to — if the server is at home that is its address on your
local network, **not** `boards.<your-domain>`: step 15 deliberately forwards port 443 only, so
port 22 is not reachable from outside.

```
ssh -N -L 8788:127.0.0.1:8788 <you>@<your-server>
# then open http://127.0.0.1:8788/?token=<your token> in a browser on your own machine
# `http` on 127.0.0.1 is the one exception browsers make to the rule in the box near the top:
# a page served from your own machine is always treated as secure.
```

If a board opens and looks right, your backups work. Stop the second server with Ctrl-C
afterwards.

> ⚠️ **A restore drill reads the backup; it must never write to it.** `velmd serve` opens
> boards with SQLite, which writes a small `-wal` file beside each one — harmless on a copy
> you are testing, and the reason this drill points at the *backup* and never at the Mac.

**28. YOU — offsite, when you are ready.** A fire or a burglary takes both disks in the same
minute. An encrypted weekly copy to cheap online storage covers that. Ask and we will write
that step for whichever provider you pick.

---

## Part 6 — Let the Mac sync with the server

Everything up to here gets your boards **onto** the server and into a browser. This is what
keeps them in step afterwards, so a change made on the Mac appears in the browser and a change
made in the browser comes back — without copying anything by hand again.

**You only do this once.** After it, the Mac just works.

⚠️ **Do Part 5 first.** This is the first thing in the whole guide that changes a board
automatically, and a backup you have actually restored is what makes that safe to switch on.

### Step 31 — Tell the Mac where the server is (YOU)

Open **Terminal on your Mac** — not the server — and run these two lines, replacing the
address with your own and the long token with the one from step 12:

```bash
echo 'export VELM_SYNC_TOKEN="paste-your-token-here"' >> ~/.zshrc
source ~/.zshrc
```

⚠️ **The token goes in an environment variable and there is deliberately no option for it.**
Anything you type as a command-line option is visible to every other program on the machine,
and this token is the only thing standing between a stranger and every board you own.

### Step 32 — Start Velm with syncing on (YOU)

```bash
/Applications/Velm.app/Contents/MacOS/vellum-app --sync-server https://boards.YOURDOMAIN.com
```

**What should happen:** Velm opens normally. Nothing looks different — that is correct. Within
about three seconds the board you have open and the one on the server have agreed with each
other.

**To check it worked:** move a sticky on the Mac, wait five seconds, then reload the board in
your browser. It should have moved there too. Then do the reverse if you have a second
computer.

**If a red message appears saying `Sync: …`,** the sentence after the colon is the actual
problem — `connection refused` means the server is not running or the address is wrong,
`401` means the token does not match. It appears **once**, not every few seconds.

### Step 33 — Make it the normal way you open Velm (YOU)

Clicking the Velm icon in your Dock does **not** pass the option, so it opens without syncing.
Two ways to fix that, and the first is simpler:

**Either** make a small launcher: open **Script Editor**, paste this, and save it as an
Application called `Velm` on your Desktop —

```applescript
do shell script "export VELM_SYNC_TOKEN='paste-your-token-here'; \
open -a Velm --args --sync-server https://boards.YOURDOMAIN.com"
```

**Or** just run the Terminal line from step 32 whenever you want syncing, and click the icon
when you do not. Both are fine; nothing breaks either way, because a board that has been
offline for a week catches up the moment it next syncs.

### What syncing does and does not do

- **Nothing is ever deleted by syncing.** A Loro update can only *add* what it knows; it has
  no way to remove what it has not seen. Two computers that disagree end up with both sets of
  changes, never with one wiped.
- **Undo stays yours.** Pressing ⌘Z on the Mac can never undo something done on another
  computer. That is deliberate: a local undo deleting somebody else's work is the one way this
  kind of syncing can still lose something.
- **It waits while you are typing.** A change arriving from the server never interrupts a word
  you are in the middle of; it lands the moment you finish.
- **A board with no file has nothing to sync** — a bench board, or an import you have not
  saved. It is simply left alone.
- **Only the board in front syncs.** Boards in your other tabs catch up when you switch to
  them, which takes about three seconds and needs nothing from you.

## The short list to remember

- **The Mac's boards are never touched by any of this.** Keep them. Forever.
- **Never add `--delete` to an rsync involving your boards.**
- **Only `velmd manifest` and `velmd verify` may ever be pointed at the Mac's own folder.**
  Step 7 does, and it is safe: neither opens a database, they only read and hash. **Never
  point `velmd serve` or `velmd import` at it.** Those open boards with SQLite, and two
  programs writing one board file at the same time is the one thing that genuinely corrupts
  one. `velmd serve` refuses that directory by name rather than trusting this sentence.
- **Never expose the server without `VELMD_TOKEN` set** (step 12). velmd refuses to start
  that way, before the socket opens — but know why the refusal is there.
- If something looks wrong, **stop and ask before tidying anything up.** Nothing here is
  urgent enough to risk a board over.

## Where things live on the server

```
/srv/velm/data/              the data directory copied from the Mac
/srv/velm/data/boards/       your boards, one .vellum file each  ← --data points HERE
/srv/velm/data/blobs/        your images, stored once each by content
/srv/velm/data/archives/     your Miro .rtb backups, carried across with everything else
/srv/velm/secret/token       the 32 random bytes that guard all of it
/srv/velm/secret/velmd.env   the same value, in the form systemd reads
/srv/velm/incoming/          the untouched copy that came from the Mac
~/velm/web/dist/             the browser client, built by scripts/build-web.sh
/mnt/<your-backup-disk>/     nightly backups, one dated folder each
```

## What is built, and what is not

Written down so nothing above reads as a promise it does not keep:

| | |
|---|---|
| Open your boards from any computer, over HTTPS | **built** |
| iPad, Safari, one finger to pan and two to pinch | **built** (iPadOS 26+) |
| Stickies, frames, text, pictures, pen strokes, connectors | **built** |
| Shapes — all 41, including the flowchart forms | **built** |
| Bold, links and per-run text colour | **built** |
| Tables, charts, mind maps and kanban boards | **built** |
| The ↗ button on a link card, and ▶ on a video card | **built** |
| A list of your boards to pick from | **built** |
| Two-way syncing between the Mac and the server | **built** — Part 6, and Part 2 is only the first crossing |
| The browser keeping up on its own, without a reload | **built** |
| A "Live" indicator, so you can tell when it has stopped keeping up | **built** |
| Editing a board in a browser | **not built** — the desktop app only. This is on purpose: a browser tab can be closed by the phone or the iPad with no warning and no chance to save, and a tab holding the only recent copy of a board is exactly what must not happen |
| The Agent Canvas in a browser | **not built**, and deferred by choice |
| Miro import in a browser | **not built** — it needs the desktop app's importer |
