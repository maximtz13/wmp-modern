# wmp-modern

A modern desktop music visualizer for Windows. Captures whatever's playing on your system (Spotify, Chrome/YouTube, YouTube Music, anything) and renders an audio-reactive 3D visualization on top of it.

Inspired by the old Windows Media Player visualizers (especially WhiteCap), but built from scratch with a modern stack.

## Features

- **System-wide *or* per-app audio capture** via Windows WASAPI loopback. Pick "System (all audio)" to visualize everything, or pick a specific app (Spotify, Chrome, Discord, etc.) to react only to that app's output.
- **Five visualizer presets** — switch between Spike (3D spectrum-ball), Canyon (synthwave flythrough over scrolling terrain with a deep U-shape and lateral camera sweep), Tunnel (180 rings receding to infinity, with beats indenting the surface and spreading across rings), Wave (current spectrum line cloned into a vertical stack of fading copies), and Bars (radial 3D equalizer). Each preset has tuned camera + audio mapping.
- **Now Playing overlay** — pulls track title, artist, album, art, and progress from Windows SMTC. Follows the source picker — picking Spotify shows Spotify's track; picking Chrome shows the Chrome tab's title.
- **Real-time spectrum analysis** — 1024-point FFT at 48 kHz, ~90 frames/second updates.
- **Small footprint** — ~10 MB executable, Tauri-based (Rust backend + Svelte frontend + Edge WebView2).

## Install

Grab the latest installer from the [Releases](../../releases) page:

- **`wmp-modern_<version>_x64-setup.exe`** — NSIS installer. Installs per-user (no admin required). Recommended.
- **`wmp-modern_<version>_x64_en-US.msi`** — MSI installer. Installs system-wide (requires admin).

### Heads-up: Windows SmartScreen warning

These binaries are **not code-signed** (this is a hobby project and commercial code-signing certs cost $100s/year). The first time you run the installer, Windows SmartScreen will show:

> **Windows protected your PC**
> Microsoft Defender SmartScreen prevented an unrecognized app from starting...

Click **More info → Run anyway**. This is a known limitation of unsigned binaries — every unsigned app on GitHub gets this warning. After the first run, Windows remembers it as trusted.

If you have **Smart App Control** enabled (a stricter Windows 11 feature), unsigned apps are blocked entirely and you'd need to disable SAC to run them. Most users do not have SAC on.

## Usage

Launch the app — it auto-starts audio capture. Play music in any app (Spotify, browser, etc.) and the visualizer reacts.

- **F** — toggle fullscreen
- **Source dropdown** (top-right HUD) — pick which app to capture from. Selection persists across launches.
- The HUD and Now Playing card auto-hide after 3s; move the mouse to bring them back.

### App compatibility for Now Playing

The Now Playing overlay reads from Windows SMTC (System Media Transport Controls). What you get depends on how well each app publishes there:

| App | Title / Artist | Album art | Position / duration |
|---|---|---|---|
| Spotify (desktop) | ✅ | ✅ | ✅ |
| Edge (YouTube, YT Music) | ✅ | ✅ | ✅ |
| Groove, Films & TV | ✅ | ✅ | ✅ |
| Chrome (YouTube) | ✅ | ✅ | ⚠️ flaky — Chrome's SMTC implementation often reports stale or zero positions |
| VLC, foobar, MPC-HC | depends on settings | depends | depends |

If an app doesn't publish to SMTC at all, the Now Playing card simply doesn't appear — the visualizer still works.

## Build from source

### Prerequisites

- Windows 10 (build 20348+) or Windows 11
- [Rust](https://rustup.rs) — stable MSVC toolchain
- [Node.js LTS](https://nodejs.org)
- Visual Studio Build Tools 2022 with the "Desktop development with C++" workload (or full Visual Studio)

### Build

```powershell
git clone https://github.com/YOUR-USER/wmp-modern.git
cd wmp-modern
npm install
npm run tauri dev      # run in dev mode
npm run tauri build    # produce installers in src-tauri/target/release/bundle/
```

## How it works

- **Audio capture** (Rust, [`src-tauri/src/audio.rs`](src-tauri/src/audio.rs)) — uses the [`wasapi`](https://crates.io/crates/wasapi) crate to open the default render endpoint in loopback mode. A dedicated capture thread pulls frames at the device's native rate, downmixes stereo to mono, applies a Hann window, runs a 1024-point FFT ([`rustfft`](https://crates.io/crates/rustfft)) every 512 samples (50% overlap → ~90 Hz update rate), then emits a Tauri event with the magnitude spectrum, raw waveform, and RMS.
- **Visualizer** (Svelte + WebGL2, [`src/routes/+page.svelte`](src/routes/+page.svelte)) — listens for spectrum events, smooths the audio features (bass / mid / treble bands, overall level, kick detector), then renders each frame: trail-fade quad pass, dim starfield, Fibonacci-sphere ray ball (600 lines), and glowing tip points. All line geometry is rebuilt on the CPU per frame and uploaded as `DYNAMIC_DRAW` VBOs. ~5 draw calls per frame.

## Tech stack

- [Tauri 2](https://tauri.app) — desktop shell (Rust + WebView2)
- [SvelteKit 2 + Svelte 5](https://svelte.dev) — frontend
- [WebGL2](https://www.khronos.org/webgl/) — rendering
- [wasapi-rs](https://crates.io/crates/wasapi) — Windows audio capture
- [rustfft](https://crates.io/crates/rustfft) — FFT

## Roadmap

- **v0.1** — system-wide audio capture + spike-ball visualizer.
- **v0.2** — per-app audio picker via `IAudioClient`'s per-process loopback mode.
- **v0.3** — Now Playing overlay via SMTC, coupled to the source picker.
- **v0.4** *(current)* — five visualizer presets (Spike / Canyon / Tunnel / Wave / Bars) with per-preset camera tuning, audio reactivity, and palette behavior. Preset choice persists in localStorage.
- *Future ideas* — backgrounds, palette customization, fullscreen-on-launch flag, beat-synced preset transitions.

## License

MIT — see [LICENSE](LICENSE).
