# Discord Rich Presence for MyStremio

A port of the Rich Presence engine from
[Loukious/stremio-shell-ng](https://github.com/Loukious/stremio-shell-ng) onto the
[MyStremio](https://github.com/xAlphiiJr/MyStremio) shell (2.4.3).

---

## Why this exists

MyStremio already had a Discord Rich Presence, but it was a much simpler thing:
it scraped the title out of the DOM, showed a **static generic Stremio icon**,
and had no buttons or metadata. Loukious' version is the one people actually
recognise — real poster art, episode names, clickable links.

This change replaces MyStremio's renderer with Loukious' one, in place. The
Settings page toggles you already use keep working exactly as before.

### Before vs after

| | MyStremio 2.4.3 | This port |
|---|---|---|
| Large image | static Stremio logo | **real poster** (Cinemeta / Anime-Kitsu, via images.weserv.nl) |
| Small image | none | **play / pause badge** |
| Title line | DOM-scraped text | series name + **real episode name** |
| State line | `Watching` / `Paused` | `S1E3`, or release year for movies |
| Buttons | none | **IMDb / Kitsu** + **Open in Stremio** |
| Timestamps | parsed from seek-bar labels | **exact `time-pos` / `duration` straight from mpv** |
| Browsing a title | not shown | **`/detail/` presence** ("Viewing Series") |
| Watch party | none | **Discord Party + "Join Watch Party"** |
| Config | 3 toggles | 3 toggles **+ `RPCconfig.ini`** |
| Update model | rebuilt from DOM every 3s | native background thread, event-driven |

---

## Files changed

```
stremio-shell/stremio-shell-ng-main/
├── Cargo.toml                                    +1 dep (rust-ini)
├── RPCconfig.example.ini                         NEW - documented reference config
├── assets/custom_discord_presence.js             rewritten (thin route reporter)
└── src/stremio_app/
    ├── discord_presence.rs                       rewritten (the port)
    └── stremio_player/player.rs                  +mpv taps for time/duration/pause
```

Everything else — `custom_api/mod.rs`, the Settings UI, the bootstrap script — is
untouched. The two IPC methods (`update-discord-presence`, `clear-discord-presence`)
keep the same names and signatures, so nothing else in the app had to change.

> All required crates (`discord-rich-presence 1.1.0`, `reqwest` blocking,
> `urlencoding`, `once_cell`) were **already** in MyStremio's `Cargo.toml`.
> `rust-ini` is the only new dependency, and it's the same version upstream uses.

---

## How it works

Upstream reads playback state from its own native mpv statics and the WebView2
URL. MyStremio doesn't have those, so the port keeps the same engine but feeds it
from two places:

```
 mpv event loop  ──(time-pos, duration, pause)──┐
                                                ├──►  discord_presence.rs
 webui JS bridge ──(current route, toggles)─────┘      (background thread)
                                                              │
                                     Cinemeta / Anime-Kitsu ◄──┤ on route change
                                                              ▼
                                                        Discord IPC
```

Playback timestamps now come straight out of the mpv event loop instead of being
parsed from the seek-bar text, so the "remaining" time in Discord matches the
player exactly. The JS file dropped from a 3-second scrape-and-diff poll to a
route reporter that only speaks up when you navigate or flip a setting.

---

## Getting the build

This is a Windows-only app (WebView2 + libmpv + native-windows-gui), so it has to
be compiled on Windows.

### Option A — let GitHub build it (no local setup)

A workflow is included at `.github/workflows/build-discord-rpc.yml`.

1. Push this branch to your fork.
2. Go to the **Actions** tab → **Build MyStremio (Discord Rich Presence)** → **Run workflow**.
3. When it finishes, download the **`mystremio-shell-discord-rpc`** artifact.

It fetches `libmpv-2.dll` automatically, and runs `cargo fmt --check`,
`clippy -D warnings` and the unit tests before building.

### Option B — build locally

```powershell
cd stremio-shell\stremio-shell-ng-main

# build.rs needs libmpv-2.dll in the project root. Either drop
# libmpv-2_x64.zip here, or copy the DLL out of your existing install:
copy "C:\Program Files\MyStremio\libmpv-2.dll" .

cargo build --release --target x86_64-pc-windows-msvc
```

Requires the **MSVC** toolchain (Visual Studio Build Tools with the C++ workload).

### Installing it

`mystremio-shell.exe` is a **drop-in replacement**:

1. Close MyStremio (also quit it from the system tray).
2. Back up `C:\Program Files\MyStremio\mystremio-shell.exe`.
3. Copy the new `mystremio-shell.exe` over it.
4. Start MyStremio.

No reinstall needed — the webui, server, ffmpeg and your settings are untouched.

---

## Using it

Turn it on in **Settings → Discord Rich Presence** exactly as before. The three
switches (enabled / show while paused / show in menus) all still apply.

For the extras, edit **`RPCconfig.ini`** next to `mystremio-shell.exe`. It is
created automatically on first run; `RPCconfig.example.ini` is a fully commented
copy. Changes apply on the next refresh — no restart.

```ini
[Buttons]
show_buttons=true
link_target=app        ; app = stremio:///... , web = web.stremio.com

[Activity]
disable_in_menu=false
disable_when_paused=false
refresh_interval=5     ; 1-60
show_small_image=true
swap_name_and_title=false
```

The Settings toggles win when they're more restrictive, so turning "show in
menus" off in the UI behaves like `disable_in_menu=true`.

---

## Watch party

The lobby/party plumbing is ported and wired into both activity builders — when a
lobby is active the presence gains a Discord `Party` (with `3/8` member count), a
**Join Watch Party** button, and a `Watching with 2 others` suffix.

MyStremio has no watch-party transport of its own (upstream's rides on
`steam_sync.rs`/`sync_protocol.rs`, ~1,400 lines of Steam-specific networking
that has no counterpart here). So the presence side is complete and ready, and
becomes live as soon as something calls:

```rust
discord_presence::set_lobby(&party_id, &join_secret, member_count, max_size);
discord_presence::clear_lobby();
```

If you want the actual Steam-backed hosting ported across too, that's a separate
piece of work — say the word.

---

## Verification

Because this sandbox is Linux, the Windows binary itself could not be produced
here. What *was* verified:

- **Type-checks** against the real `discord-rich-presence 1.1.0`, `rust-ini`,
  `reqwest`, `urlencoding` and `flume` crates (mpv stubbed).
- **`rustfmt --check` clean** and **`clippy -D warnings` clean** — the same gates
  MyStremio's own CI enforces.
- **8 unit tests pass**, covering id parsing (movie / series / `kitsu:` / `kitsu:id:ep`),
  route extraction, IMDb + Kitsu + deep-link button generation, the
  `show_buttons=false` path, poster URL encoding, and party member text.
- **Live API check** against Cinemeta: Breaking Bad S1E3 resolved to
  `name="Breaking Bad"`, `year="2008–2013"`, `epname="...And the Bag's in the River"`
  plus a poster URL; a bogus id correctly returned `None` and fell back to the
  menu presence instead of going dark.
- **Config round-trip**: auto-creation, honouring hand-edits, clamping
  `refresh_interval=900` → `60`, and surviving a corrupted ini without panicking.
- The image proxy (`images.weserv.nl`) returned HTTP 200 for a real metahub poster.

Not verifiable from here: linking against real libmpv/WebView2, and the Discord
IPC handshake itself (needs a running Discord client).

One thing to know: **Anime-Kitsu's endpoint (`anime-kitsu.strem.fun`) returned
HTTP 403 from this sandbox** — a Cloudflare block on the datacenter IP, not a code
fault. It should resolve normally from a home connection, and the code now sends
a User-Agent, checks the HTTP status, and won't cache a failed lookup so it
retries on the next navigation.
