# Harmony

**Self-hosted, low-latency screen sharing and voice chat for a group of friends.**
A small Discord you run yourself: voice channels with screen and camera sharing,
text channels, and sub-second latency, because the server only relays video and
never re-encodes it.

This is a fork of [Harmony by Pedro Lucas Miguel](https://github.com/PedroLucasMiguel/harmony).
Pedro built the project: the server, the protocol and the original Electron
client. This fork keeps his server, with a few small additions that stay
compatible, and replaces the client with a native one written in Rust on
[GPUI](https://www.gpui.rs/), the UI framework behind the Zed editor.

## What is different in this fork

The client is one native `harmony.exe`, with no browser engine inside. It talks
to the same server as Pedro's version and reads the same settings file, so an
upgrade keeps your server, your sign-in and your preferences.

The rest is new. A rail on the left keeps every server you joined, one click
apart. Admins and the owner get a server settings page, and what only the owner
may do shows with a lock instead of hiding. Turning the camera on opens a
preview where you pick the camera and the background first. Your profile has its
own page, the app speaks English and Portuguese, and an Update button shows up
when a new release is out. The voice code reconnects cameras and screen shares
by itself after a network blip.

On the server side, `HARMONY_SIGNALING_URL=auto` hands each client the address it
actually used, so people on the LAN and on a VPN can share one server, and the
owner can give the server a picture.

## Features

- **Screen or window sharing** at up to native resolution and 120 fps, H.264
  encoded on your GPU when it has an encoder. The sound goes with it: the whole
  computer except the call, or just the shared app.
- **Voice channels** with cameras and screen shares, echo cancellation and
  noise suppression, per-person volume up to 350%, mute, deafen and a
  soundboard anyone in the call can play.
- **Camera backgrounds**: blur, six pictures that ship with the app, or one of
  your own. The person is found on your computer by a small segmentation model;
  only the finished picture is sent.
- **Text channels** with Markdown, attachments, reactions, custom emoji,
  mentions, pins and search.
- **Accounts and roles**: owner, admins and members, with optional
  password-locked channels and one settings page to run the server.
- **Several servers** on a rail, saved as you join them; right-click one to remove it.
- **English and Portuguese**, following the system or picked in Settings.
- **Runs on almost anything.** The server never decodes a frame, so a
  Raspberry Pi is plenty; upload bandwidth is the only real limit.

## Quick start

**Server** (any 64-bit Linux, x86-64 or arm64):

```bash
sudo server/install.sh
```

or with Docker. See [docker/](docker/).

**Client** (Windows): a native app in Rust and [GPUI](https://www.gpui.rs/).
With [Rust](https://rustup.rs/) and the Visual Studio Build Tools (C++
workload) installed:

```bash
cargo build --release --manifest-path client/Cargo.toml   # -> client/target/release/harmony.exe
```

Or download `harmony-<version>-windows-x64.exe` from the
[latest release](https://github.com/Brunovncs/harmony/releases/latest) and keep
it in a folder you can write to. When a newer release is out, an Update button
shows at the top of the window. It downloads the new program, checks its
SHA-256, swaps it in and restarts Harmony, waiting for your voice call to end if
you are in one. Harmony asks GitHub for the latest release when it opens and
every 6 hours; "Check for updates" in Settings, Advanced, turns that off. A build
made with `cargo run` never replaces itself.

TLS, dynamic IPs, ports and troubleshooting are covered in
**[DEPLOYMENT.md](DEPLOYMENT.md)**.

## Built on

[MediaMTX](https://github.com/bluenviron/mediamtx) ·
[GPUI](https://www.gpui.rs/) ·
[LiveKit's libwebrtc bindings](https://github.com/livekit/rust-sdks) ·
[tract](https://github.com/sonos/tract) ·
[MediaPipe selfie segmentation](https://huggingface.co/onnx-community/mediapipe_selfie_segmentation) ·
[Geist](https://vercel.com/font) ·
[Lucide](https://lucide.dev/)

## Credits and license

Harmony was created by [Pedro Lucas Miguel](https://github.com/PedroLucasMiguel).
The GPUI client and the changes in this fork are by
[Brunovncs](https://github.com/Brunovncs). Third-party material in the client is
listed in [client/THIRD_PARTY_NOTICES.md](client/THIRD_PARTY_NOTICES.md).

MIT, see [LICENSE](LICENSE).

---

## Disclaimer: vibe-coded

> [!WARNING]
> **Every line of this repository (client, server, tests and docs) was written
> by an AI coding assistant**, prompted by a human who did not review it line by
> line. It is a recreational project, built for fun and for a handful of
> friends.
>
> It works and has run for a small group, but it has had **no security review,
> no audit and no abuse controls**, and comes with **no stability guarantees**.
> Use it on a LAN or among people you trust, not for anything that matters when
> it breaks. **No warranty.**
