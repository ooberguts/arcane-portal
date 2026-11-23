# Arcane Portal

Arcane Portal shares a screen or application window with other people on your local network. It opens in **Live Rooms**, where you can watch a broadcast without sharing anything from your own computer.

## Install

Download the ZIP for your computer from [GitHub Releases](https://github.com/ooberguts/arcane-portal/releases/latest).

| Platform | Package | Setup |
| --- | --- | --- |
| Windows 10 version 2004 or newer, or Windows 11 (x64) | `Arcane-Portal-Windows-x64.zip` | Extract the ZIP and open `Arcane Portal.exe`. |
| macOS 13 or newer (Apple silicon) | `Arcane-Portal-macOS-Apple-silicon.zip` | Extract the ZIP and move `Arcane Portal.app` to Applications. |
| macOS 13 or newer (Intel) | `Arcane-Portal-macOS-Intel.zip` | Extract the ZIP and move `Arcane Portal.app` to Applications. |

The Windows executable is unsigned. The macOS apps are ad-hoc signed and have no Apple notarization. macOS may ask for Local Network access. To share from a Mac, allow Arcane Portal in **System Settings → Privacy & Security → Screen & System Audio Recording** (called **Screen Recording** on older macOS versions), then reopen the app if prompted. On Windows, allow the app through the firewall for **private networks** if other devices cannot connect.

## Watch a broadcast

1. Open Arcane Portal on computers on the same local network.
2. Live broadcasts appear automatically in **Live Rooms**. Select **Watch** next to the broadcast you want to join.
3. Open the stream in fullscreen or leave it at any time.

## Share your screen

1. Open **Settings** to set the display name people see in Live Rooms. Arcane Portal uses your computer name by default.
2. Open **Broadcast**, choose a screen or window, pick a quality preset, and choose **Go live**. A local preview shows what the viewers receive.
3. Viewers on your network can find the broadcast in **Live Rooms**. **Stop broadcast** ends the stream for everyone.

Quality presets target 480p at 15 fps, 720p at 24 fps, or 1080p at 30 fps. Actual resolution depends on the source and its aspect ratio. Capture is video-only.

## Network behavior

Rooms are open to Arcane Portal clients on a directly connected private IPv4 subnet. Stream traffic is unencrypted, so use it on a trusted network. The app uses mDNS discovery (UDP 5353) and a direct TCP connection, normally on port 48742. Arcane Portal runs without accounts, cloud services, or internet relays.

## Build from source

Install stable Rust. Use Visual Studio Build Tools with the Desktop development with C++ workload on Windows, or Xcode 26 or newer on macOS. The Mac packages target macOS 13 and later.

```sh
cargo test --locked
cargo run --release --locked
```

The [Desktop builds workflow](https://github.com/ooberguts/arcane-portal/actions/workflows/desktop.yml) runs tests, builds the three packages, and publishes them when a version tag is pushed. Arcane Portal uses Windows Graphics Capture and ScreenCaptureKit, with an `egui` desktop interface. Its named LAN rooms and automatic discovery are inspired by [Elysium](https://github.com/ooberguts/Elysium).
