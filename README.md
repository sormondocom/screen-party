# Screen Party

<p align="center">
  <img src="assets/mascot.svg" alt="Screen Party mascot — a sun broadcasting sound waves and laser beams to Earth" width="320"/>
</p>

<p align="center">
  <a href="https://buymeacoffee.com/sormondocom"><img src="https://img.shields.io/badge/Buy%20Me%20a%20Coffee-support%20the%20project-yellow?logo=buy-me-a-coffee" alt="Buy Me a Coffee"/></a>
</p>

**Free, secure, self-hosted screen and audio sharing.**

No accounts. No cloud relay. No subscription. Run the host on your machine, share the address, and anyone with the client can watch your screen and hear your audio in real time.

---

## Quick start

**On the sharing machine:**
```
party host
```
A fullscreen overlay appears. Drag to select the region you want to share, then release the mouse or press Enter. Sharing begins immediately. Share your IP address with your viewers.

**On each viewer machine:**
```
party client --host 192.168.1.10
```
A window opens and fills with the host's stream as soon as it arrives. That's it.

---

## Host walkthrough

### 1. Start the host

```
party host
```

- Listens on port 7777 by default.
- The fullscreen selection overlay appears on startup.
- Drag to draw a rectangle over what you want to share, then release the mouse (or press Enter) to start broadcasting.

| Action | Effect |
|--------|--------|
| Drag + release | Start capture of selected region |
| Enter | Confirm current selection |
| Esc (during drag) | Cancel the drag |
| Esc Esc (double, within 400 ms, while capturing) | Stop capture and go back to selection |
| Ctrl+Q | Quit |

### 2. Chat

Once capture starts, a **chat window** opens in the corner of your screen. Type there to send messages to all viewers. Viewers can reply from their viewer window.

System events (connections, approvals, disconnections) appear in **amber** so they stand out from regular chat.

### 3. Admin commands

Type any of these in the host chat window and press Enter:

| Command | Effect |
|---------|--------|
| `/approve <fp>` | Let a pending client through |
| `/deny <fp> [reason]` | Reject a pending client; the reason is shown to them |
| `/kick <fp>` | Disconnect a currently-streaming viewer |
| `/viewers` | List who is watching and who is pending |
| `/help` | Print the command list |

`<fp>` is the first 8 characters of the client's fingerprint, shown in the `[JOIN]` notification.

### 4. Host options

```
party host --port 8888          # listen on a different port (default: 7777)
party host --generate-key       # create or replace your PGP identity
party host --cache-secs 5       # shrink the stream cache (default: 10 s, see below)
```

### 5. Stream cache (`--cache-secs`)

The host keeps a rolling ring buffer of the last N seconds of encoded video and audio. New clients receive this buffer immediately on connect — eliminating the blank "connecting" wait and pre-seeding their playback buffer before going live. Slow clients read the ring at their own pace without causing anyone else to drop frames.

**Memory budget**

The cache holds zstd-compressed delta frames and raw f32 audio. How much RAM it uses depends almost entirely on screen activity — the quadtree encoder only transmits changed pixels, so a static screen costs nearly nothing.

| Sharing scenario | ~MB per second | ~MB at default (10 s) |
|---|---|---|
| Static / mostly-static screen | < 1 | < 10 |
| Code editing, slides, documents | 1–5 | 10–55 |
| Scrolling, UI animations | 5–20 | 55–205 |
| Full-screen HD video playback | 50–100 | 500–1,000 |

Audio adds a flat ~0.4 MB/s in all cases (raw f32 PCM at 48 kHz stereo) and can be ignored in any budget calculation.

**Rules of thumb:**
- **Presentation or code sharing** — the default `--cache-secs 10` uses roughly **20–100 MB**. No tuning needed.
- **Full-screen video content** — drop to `--cache-secs 2` or `3` to stay under ~100 MB.
- **Low-memory host (< 2 GB RAM)** — `--cache-secs 3` is a safe ceiling for any content type.

---

## Client walkthrough

### 1. Connect

```
party client --host 192.168.1.10
party client --host 192.168.1.10 --port 8888   # if the host uses a non-default port
```

The viewer window opens and scales the stream to fill it. Audio plays automatically.

### 2. Chat

The bottom strip of the viewer window is always the chat panel. Just start typing — no need to click anywhere. Press Enter to send.

Your identity and the host's identity are shown at the top of the chat panel:
```
You: abc123ef   Host: def456gh [known]
```

`[known]` means this host's fingerprint matches what was recorded the first time you connected. `[new]` means this is your first connection to this host.

### 3. Viewer controls

| Action | Effect |
|--------|--------|
| Type + Enter | Send a chat message |
| Backspace | Delete last chat character |
| Escape | Open the disconnect menu |
| D (in menu) | Disconnect and close |
| C or Escape (in menu) | Close the menu |

### 4. Client options

```
party client --host 192.168.1.10 --name "Alice"  # set your display name in chat
party client --host 192.168.1.10 --port 8888     # connect to a non-default port
```

---

## Identity and security

### How encryption works

Every session is end-to-end encrypted — no configuration required. When a client connects, the host and client perform an **X25519 Diffie-Hellman key exchange** and derive a unique session key. All subsequent traffic (video, audio, chat) is encrypted with **ChaCha20-Poly1305**. Session keys are ephemeral and never stored.

### Fingerprints and identity

Run this once to generate a persistent identity:

```
party host --generate-key
```

You will be prompted for a nickname and an optional passphrase. The resulting key is saved to `~/.screen-party/identity.asc`. Your **fingerprint** — a hex string derived from your public key — is your stable identity across sessions.

- The host's fingerprint is shown in the **host chat window** at all times (`Your ID: …`). Share it with your viewers out-of-band (Signal, email, in person) before the session so they can verify it.
- The client's fingerprint and the host's fingerprint are both shown at the top of the **viewer window** chat panel.
- Your fingerprint appears in chat as `Name [fp6]` (your chosen name followed by the first 6 hex characters of your fingerprint).

### Fingerprint confirmation (always on)

Every time a client connects to a host they have **not seen before**, they are shown the host's full fingerprint and must type `yes` to proceed. The fingerprint is then saved to `~/.screen-party/known_hosts`.

On **subsequent connections** to the same host:
- If the fingerprint **matches** → auto-accepted; the viewer sees `[known]` in their chat panel.
- If the fingerprint **has changed** → the connection is refused immediately with a warning. This protects against man-in-the-middle attacks.

### Client approval gate (always on)

Every connecting client is **held after the speed test** — they cannot see the stream until the host approves them. The host chat window shows:

```
[JOIN] Alice [abc123ef]  →  /approve abc123ef  or  /deny abc123ef [reason]
```

Type the command in the chat window and press Enter. The client waits up to 5 minutes before timing out.

---

## Features

- **End-to-end encryption** — X25519 key exchange, ChaCha20-Poly1305 AEAD, per-session ephemeral keys
- **PGP identity** — stable fingerprint across sessions, saved as an armored key in `~/.screen-party/`
- **Known hosts** — automatic fingerprint pinning; changed fingerprints are refused
- **Client approval gate** — every viewer is held until the host approves; approve, deny, or kick via chat commands
- **Display names** — clients set `--name "Alice"`; shown as `Alice [fp6]` in chat
- **Screen capture** — drag-to-select region, 30 fps, cyan border overlay while capturing
- **Delta compression** — quadtree dirty-rect detection + zstd; only changed pixels are transmitted
- **System audio** — Windows WASAPI loopback capture; no virtual cable needed
- **Multi-client fanout** — one host, unlimited simultaneous viewers
- **Bidirectional chat** — host and all viewers share a single chat room
- **Host-side stream cache** — rolling ring buffer of encoded frames; new clients are seeded with the tail before going live; slow clients read at their own pace without affecting others
- **Speed probe** — jitter-measured pre-buffering sized to actual link conditions
- **Video playback buffer** — client-side frame queue absorbs network jitter; prebuffer depth auto-sized from the speed probe; hard ceiling drops oldest frames to stay within ~4 s of live
- **Viewer reconnect** — re-select region with double-Esc without restarting the host

---

## Platform support

| Platform | Screen capture | Audio |
|----------|---------------|-------|
| Windows | DXGI Desktop Duplication | WASAPI loopback (no virtual cable) |
| macOS | planned | planned |
| Linux | planned | planned |

---

## Building

Requires [Rust](https://rustup.rs/) (stable, 1.75+) and a C compiler (for the zstd native library).

```
git clone <repo>
cd screen-party
cargo build --release
```

The release binary is `target/release/party.exe` on Windows. The Windows release build uses the GUI subsystem — no console window appears.

---

## Architecture

```
crates/
  capture/   — DXGI screen capture, quadtree dirty-rect detector, region selector UI
  audio/     — WASAPI loopback capture, cpal playback with jitter buffer
  party/     — binary: phase state machine, broadcaster, TCP protocol, chat UI, identity
```

The wire protocol is a simple length-prefixed framing (`[u32 len][u8 type][payload]`), transparently wrapped in per-session ChaCha20-Poly1305 AEAD records after the X25519 handshake. Video payloads are zstd-compressed dirty-rect bundles. Audio is raw interleaved f32 PCM. Chat and control messages are UTF-8 with length-prefixed fields. Protocol version is checked at handshake; version mismatches disconnect cleanly.

---

## Roadmap

- [ ] macOS and Linux platform implementations
- [ ] H.264 / hardware-accelerated video encoding
- [ ] Adaptive bitrate / FPS based on ongoing link quality
- [ ] Multi-monitor selection UI
- [ ] Client reconnect on connection drop
- [ ] NAT traversal / relay for internet sessions

---

## Support

If Screen Party is useful to you, you can buy the developer a coffee at [buymeacoffee.com/sormondocom](https://buymeacoffee.com/sormondocom).
