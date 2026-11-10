# Raidwide OBS plugin

OBS plugin for [Raidwide](https://raidwi.de), video analyzer for Final Fantasy XIV raids. It reads the game's combat log from ACT and OverlayPlugin, and embeds it in your recording or live stream, so that Raidwide can split the session into pulls and line up every player's point of view.

## Install

Download `raidwide_ffxiv_rs.dll` from the [latest release](../../releases/latest), and copy it to `C:\Program Files\obs-studio\obs-plugins\64bit\`. It needs OBS 31 or newer.

Then, with ACT and OverlayPlugin running:

1. Add a **Raidwide** source to your scene collection.
2. In its properties, check the **WebSocket URL** of OverlayPlugin's WebSocket server (`ws://127.0.0.1:10500/ws` by default), and click **Test Connection**.
3. Record or stream as usual: the combat log is embedded in the video.

## How it works

The plugin hooks OBS' outputs and inserts the events it receives from OverlayPlugin in the encoded video, without re-encoding: as user data SEI messages in H.264 and HEVC, as a metadata OBU in AV1, all starting with the UUID `7261696477696465FF14FF14FF14FF14` ("raidwide" followed by `FF14` four times). They survive recording to a file and streaming over SRT, and viewers don't see them. The framing of each codec is described in [`raidwide-ffxiv-rs/README.md`](raidwide-ffxiv-rs/README.md).

Raidwide only analyzes H.264 videos for now: keep OBS' encoder on H.264 (x264, NVENC H.264, ...).

## Build

The OBS headers come from the `obs-studio` submodule:

```sh
git clone --recursive https://github.com/raidwide/raidwide-obs
cd raidwide-obs
cargo build --release
```

On Linux, it needs libobs 31 or newer (e.g. from the `ppa:obsproject/obs-studio` PPA on Ubuntu) and libclang. On Windows, an installed OBS of the same version as the submodule; see `.github/workflows/ci.yml` for the exact steps.

## License

GPL-2.0, as OBS, which the plugin links against. See `LICENSE`.
