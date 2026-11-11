# Raidwide OBS plugin

OBS plugin for [Raidwide](https://raidwi.de), video analyzer for Final Fantasy XIV raids. It reads the game's combat log from ACT and OverlayPlugin, and embeds it in your recording or live stream, so that Raidwide can split the session into pulls and line up every player's point of view.

## Install

Download `raidwide_ffxiv_rs.dll` from the [latest release](../../releases/latest). It needs OBS 31 or newer. Copy it, keeping its name, to:

- OBS 33 or newer: `C:\ProgramData\obs-studio\plugins\raidwide_ffxiv_rs\raidwide_ffxiv_rs.dll`
- OBS 31 or 32: `C:\ProgramData\obs-studio\plugins\raidwide_ffxiv_rs\bin\64bit\raidwide_ffxiv_rs.dll`

Create the folders if needed. Older guides mention `C:\Program Files\obs-studio\obs-plugins\64bit\`: OBS 33 flags plugins there as legacy, and OBS 34 won't load them. See OBS' [plugin locations](https://obsproject.com/kb/legacy-plugin-developer-info).

Then, with ACT and OverlayPlugin running:

1. Add a **Raidwide** source to your scene collection.
2. In its properties, check the **WebSocket URL** of OverlayPlugin's WebSocket server (`ws://127.0.0.1:10500/ws` by default), and click **Test Connection**.
3. Record or stream as usual: the combat log is embedded in the video.

## Build

The OBS headers come from the `obs-studio` submodule:

```sh
git clone --recursive https://github.com/raidwide/raidwide-obs
cd raidwide-obs
cargo build --release
```

On Linux, it needs libobs 31 or newer (e.g. from the `ppa:obsproject/obs-studio` PPA on Ubuntu) and libclang. On Windows, an installed OBS of the same version as the submodule; see `.github/workflows/ci.yml` for the exact steps.

## License

GPL-2.0-or-later (see `LICENSE`). OBS Studio is under the same license, and plugins that link against it must be GPL-compatible.
