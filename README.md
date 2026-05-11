# Screen Party

[![Buy Me a Coffee](https://img.shields.io/badge/Buy%20Me%20a%20Coffee-support%20the%20project-yellow?logo=buy-me-a-coffee)](https://buymeacoffee.com/sormondocom)

<p align="center">
  <img src="assets/mascot.svg" alt="Screen Party mascot — an angel with laser eyes broadcasting to Earth" width="320"/>
</p>

**Free, secure, self-hosted screen and audio sharing.**

No accounts. No cloud relay. No subscription. Run the host on your machine, share the address, and anyone with the client can watch your screen and hear your audio in real time.

---

## Goals

- **Free** — open source, no licensing costs, no usage limits
- **Secure** — PGP-based identity and fingerprint verification; no third-party servers involved in your session
- **Self-hosted** — direct TCP connection from host to clients; your data never touches an intermediary
- **Low friction** — drag to select a region, press Enter, done

---

## Features

- Drag-to-select screen region capture with a live selection overlay
- System audio loopback capture streamed alongside video (Windows WASAPI; no virtual cable needed)
- Delta-only video transmission — only changed screen regions are sent each frame, compressed with zstd
- Multi-client fanout — one host, many simultaneous viewers
- Built-in chat — bidirectional text between host and all connected clients
- PGP identity fingerprints — optionally require viewers to interactively confirm the host's key before connecting
- Connection speed probe — jitter-adaptive audio buffering sized to actual link conditions
- 30 fps cap with sub-frame sleep to keep CPU and bandwidth predictable

---

## Platform support

| Platform | Screen capture | Audio loopback |
|----------|---------------|----------------|
| Windows  | DXGI Desktop Duplication | WASAPI loopback (no virtual cable) |
| macOS    | planned | planned |
| Linux    | planned | planned |

---

## Building

Requires [Rust](https://rustup.rs/) (stable, 1.75+) and a C compiler (for the zstd native library).

```
git clone <repo>
cd screen-party
cargo build --release
```

The release binary is `target/release/party` (or `party.exe` on Windows). The Windows release build uses the GUI subsystem — no console window appears.

---

## Usage

### Host

```
party host
```

Starts listening on port 7777. A fullscreen overlay appears — drag to select the region you want to share, then release the mouse (or press Enter) to begin capture.

| Action | Effect |
|--------|--------|
| Drag + release | Start capture of selected region |
| Esc Esc (double, within 400 ms) | Stop capture and reselect |
| Ctrl+Q | Quit |

**Options:**

```
party host --port <PORT>        # listen on a different port (default: 7777)
party host --generate-key       # create or replace your PGP identity
party host --interactive        # require clients to confirm your fingerprint
```

### Client

```
party client --host <IP or hostname>
```

Opens a viewer window that scales to fill as the host's stream arrives. Chat input is always active at the bottom of the window. Press Escape to open the disconnect menu.

```
party client --host 192.168.1.10 --port 7777
party client --host 192.168.1.10 --interactive   # verify host fingerprint before connecting
```

### Identity / key management

```
party host --generate-key
```

Generates a PGP keypair stored locally. The first 8 characters of your public key fingerprint become your display name in chat. Run once before your first hosted session if you want a stable identity.

---

## Architecture

```
crates/
  capture/   — DXGI screen capture, quadtree dirty-rect detector, region selector UI
  audio/     — WASAPI loopback capture, cpal playback
  party/     — binary: phase state machine, broadcaster, TCP protocol, chat UI
```

The wire protocol is a simple length-prefixed framing (`[u32 len][u8 type][payload]`). Video payloads are zstd-compressed dirty-rect bundles. Audio is interleaved f32 PCM. Chat is UTF-8 with sender/text length-prefixed fields. Protocol version is checked at handshake; mismatched versions disconnect cleanly.

---

## Roadmap

- [ ] macOS and Linux platform implementations
- [ ] H.264 / hardware-accelerated video encoding
- [ ] End-to-end encryption of the stream
- [ ] Multi-monitor selection
- [ ] Client reconnection on drop

---

## Support

If Screen Party is useful to you, you can buy the developer a coffee at [buymeacoffee.com/sormondocom](https://buymeacoffee.com/sormondocom).
