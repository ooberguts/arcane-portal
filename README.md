# Arcane Portal

Arcane Portal shares a screen or application window with other people on your local network. It opens in **Live Rooms**, where you can watch a broadcast without sharing anything from your own computer.

## Install

Download the ZIP for your computer from [GitHub Releases](https://github.com/ooberguts/arcane-portal/releases/latest).

| Platform | Package | Setup |
| --- | --- | --- |
| Windows 10 version 2004 or newer, or Windows 11 (x64) | `Arcane-Portal-Windows-x64.zip` | Extract the ZIP and open `Arcane Portal.exe`. |
| macOS 13 or newer (Apple silicon) | `Arcane-Portal-macOS-Apple-silicon.zip` | Extract the ZIP, move `Arcane Portal.app` to Applications in Finder, then open it. |
| macOS 13 or newer (Intel) | `Arcane-Portal-macOS-Intel.zip` | Extract the ZIP, move `Arcane Portal.app` to Applications in Finder, then open it. |

The Windows executable is unsigned. The macOS apps are ad-hoc signed and have no Apple notarization. macOS may ask for Local Network access. To share from a Mac, allow Arcane Portal in **System Settings → Privacy & Security → Screen & System Audio Recording** (called **Screen Recording** on older macOS versions), then reopen the app if prompted. On Windows, allow the app through the firewall for **private networks** if other devices cannot connect.

## Update the app

In **Settings → Updates**, check GitHub for a newer release. If an update is available, download it, then choose **Install and restart**. Arcane Portal verifies the package against the SHA-256 digest from GitHub before it installs. Extract the Windows app before using the updater, or move the macOS app to a writable Applications folder.

On macOS, a read-only update path under `/private/var/folders/` can mean the app launched from a temporary location. Quit Arcane Portal, move the extracted `.app` into Applications with Finder, and launch it from there before retrying. A writable Applications folder in your home directory also works.

## Watch a broadcast

1. Open Arcane Portal on computers on the same local network.
2. Live broadcasts appear automatically in **Live Rooms**. Select **Watch** next to the broadcast you want to join.
3. Open the stream in fullscreen or leave it at any time.

## Share your screen

1. Open **Settings** to set the display name people see in Live Rooms. Arcane Portal uses your computer name by default.
2. Open **Broadcast**, choose a screen or window, and choose **Go live**. A local preview shows what the viewers receive.
3. Viewers on your network can find the broadcast in **Live Rooms**. **Stop broadcast** ends the stream for everyone.

Set **Resolution** and **Frame rate** independently before going live or while broadcasting. Resolution can target 480p, 720p, or 1080p. Frame rate can target 15, 24, 30, or 60 FPS. Each control has an **Auto** setting that adjusts based on capture and viewer delivery. The live FPS counters show how many frames the host sends and the viewer decodes. Resolution retains the source's aspect ratio.

Turn on **Share system audio** in **Broadcast** when you want viewers to hear sound playing on your computer. You can switch it on or off while live. Viewers can toggle playback or go fullscreen from the watch screen. Update both devices for the faster video transport and audio support.

## Network behavior

Rooms are open to Arcane Portal clients on a directly connected private IPv4 subnet. Stream traffic is unencrypted, so use it on a trusted network. The app uses mDNS discovery (UDP 5353) and a direct TCP connection, normally on port 48742. If that inbound connection fails, current versions can have the broadcaster initiate a TCP connection to the viewer. Sharing and watching work offline. Update checks and downloads use GitHub.

If a room appears but cannot connect, verify that both devices are on the same trusted network. On a Windows broadcaster, use the **Private** network profile for trusted Wi-Fi and allow Arcane Portal through the firewall on that profile. On macOS, allow Local Network access when prompted.

## Build from source

Install stable Rust. Use Visual Studio Build Tools with the Desktop development with C++ workload on Windows, or Xcode 26 or newer on macOS. The Mac packages target macOS 13 and later.

```sh
cargo test --locked
cargo run --release --locked
```

The [Desktop builds workflow](https://github.com/ooberguts/arcane-portal/actions/workflows/desktop.yml) runs tests, builds the three packages, and publishes them when a version tag is pushed. Arcane Portal uses Windows Graphics Capture and ScreenCaptureKit, with an `egui` desktop interface. Its named LAN rooms and automatic discovery are inspired by [Elysium](https://github.com/ooberguts/Elysium).
