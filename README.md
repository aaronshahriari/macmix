# macmix

macmix is a terminal (TUI) audio mixer for macOS, built natively on Apple's
CoreAudio. It lets you control your audio devices and see what's playing,
without leaving the terminal:

- **Adjust per-device volume and mute** for output and input devices.
- **Switch the default output/input device** (the system "sound output/input").
- **Live VU meters** for output devices, input devices, and individual
  applications.
- **Mute individual applications** (e.g. silence one app while others keep
  playing).
- **Hide the noise** — virtual devices that other apps install (Teams, Zoom,
  AppVolume, etc.) are hidden by default.

No kernel extension, no audio driver, no background daemon — just a CLI that
talks to CoreAudio directly.

<img src="https://github.com/user-attachments/assets/26823e34-3a6f-4a3a-bdb2-cde7f3d4cbe5" width="612">

## Requirements

- **macOS 14.4 or later.** Device control works broadly, but the VU meters and
  per-application mute use the modern CoreAudio process-tap API introduced in
  macOS 14.4.
- A **Rust toolchain** (e.g. via [rustup](https://rustup.rs)) to build.

The first time macmix meters an input device it may ask for **microphone**
permission, and metering output/applications may ask for **audio-recording**
permission — these are only requested when you open a tab that needs them.

## Installation

```sh
git clone git@github.com:aaronshahriari/macmix.git
cd macmix
cargo install --path .
```

This builds an optimized binary and installs it to `~/.cargo/bin/macmix` (on
your `PATH` if you use rustup). Run `cargo install --path .` again to update
after pulling changes.

## Quick Start

Run `macmix` to launch with default settings, then:

- `?` to show the keyboard bindings
- `Tab` / `H` / `L` to switch tabs
- arrow keys or `hjkl` to navigate lists and adjust volume
- `m` to mute / unmute the selected item
- `d` to set the selected output/input device as the system default
- `q` to quit

By default, virtual devices created by other apps are hidden. Pass
`--all-devices` (or set `show_all_devices = true` in the config) to show them.

## Tabs

- **Output Devices** — your speakers/headphones/etc. Adjust volume, mute, set
  the default, and watch the output meter.
- **Input Devices** — microphones and capture devices. Same controls, plus a
  live input meter.
- **Playback** — applications currently producing audio. Each app shows a live
  meter and can be muted independently.
- **Recording** — applications capturing audio (often empty on macOS).
- **Configuration** — reserved; macmix models devices simply and does not
  expose CoreAudio profiles/routes here.

### A note on per-application volume

macmix can **mute** individual apps, but it does **not** offer per-app *partial*
volume (a 50% slider). On macOS that requires intercepting and re-rendering each
app's audio through a virtual device, which is the job of a signed audio driver
(e.g. AppVolume). macmix keeps to what it can do safely and natively, so the
per-app volume slider is display-only — use the device volume, or a dedicated
per-app volume app, for fine control.

## Command-line Options

```
A TUI mixer for macOS CoreAudio

Usage: macmix [OPTIONS]

Options:
  -c, --config <FILE>                 Override default config file path
  -f, --fps <FPS>                     Target frames per second (or 0 for unlimited)
  -s, --char-set <NAME>               Character set [built-in: default, compat, extracompat]
  -t, --theme <NAME>                  Theme [built-in: default, nocolor, plain]
  -p, --peaks <PEAKS>                 Audio peak meters [values: off, mono, auto]
      --no-mouse                      Disable mouse support
      --mouse                         Enable mouse support
  -v, --tab <TAB>                     Initial tab [values: playback, recording, output,
                                      input, configuration]
  -T, --tabs <TABS>...                Which tabs are present and their order
      --all-devices                   Show all audio devices, including virtual ones installed
                                      by other apps (e.g. Teams, Zoom, AppVolume)
  -m, --max-volume-percent <PERCENT>  Maximum volume for volume sliders
      --no-enforce-max-volume         Allow increasing volume past max-volume-percent
      --enforce-max-volume            Prevent increasing volume past max-volume-percent
      --no-lazy-capture               Meter all nodes (accesses devices even off-screen)
      --lazy-capture                  Only meter on-screen nodes (default; lower CPU)
  -h, --help                          Print help
  -V, --version                       Print version
```

Command-line options override the corresponding settings in the configuration
file.

## Input Bindings

Most actions can also be done with the mouse — notably:

- Click the numeric volume percentage to toggle muting.
- Scroll lists and dropdowns with the mouse wheel.
- Right-click a device to set it as the default.

### Default Keyboard Bindings

| Input         | Action                      |
| ------------- | --------------------------- |
| q             | Quit                        |
| m             | Toggle mute                 |
| d             | Set default output/input    |
| l/Right arrow | Increment volume            |
| h/Left arrow  | Decrement volume            |
| Enter/c       | Open dropdown / choose      |
| Esc           | Cancel dropdown             |
| j/Down arrow  | Move down                   |
| k/Up arrow    | Move up                     |
| H/Shift+Tab   | Select previous tab         |
| L/Tab         | Select next tab             |
| ` (Backtick)  | Set volume 0%               |
| 1 – 9         | Set volume 10% – 90%        |
| 0             | Set volume 100%             |
| ?             | Toggle help screen          |

## Configuration

macmix is configured through a TOML file. It looks for the file in these
locations, in order of precedence:

1. The path given on the command line via `-c`/`--config`
2. `$XDG_CONFIG_HOME/macmix/macmix.toml`
3. `~/.config/macmix/macmix.toml`

Your configuration is merged with macmix's defaults, so you only need to set the
options you want to change. The bundled [macmix.toml](./macmix.toml) documents
every option and the default values — start from an empty file and use it as a
reference.

### Basic Options

```toml
#fps = 60.0
mouse = true
peaks = "auto"
char_set = "default"
theme = "default"
tab = "playback"
tabs = [ "playback", "recording", "output", "input", "configuration" ]
max_volume_percent = 150.0
enforce_max_volume = false
lazy_capture = true
show_all_devices = false
```

### Character Sets and Themes

Character sets define the UI symbols; themes define colors and text attributes.
Both have built-ins (`default`, `compat`, `extracompat` for character sets;
`default`, `nocolor`, `plain` for themes) and can be customized or extended.
Select them with `char_set`/`theme` (or `-s`/`-t`). See
[macmix.toml](./macmix.toml) for details.

### Names

You can customize how devices and applications are labeled with a small template
system. The defaults are:

```toml
[names]
# Per-application streams (Playback/Recording tabs) — the app's name
stream = [ "{node:node.description}" ]
# Devices (Output/Input tabs)
endpoint = [ "{device:device.nick}", "{node:node.description}" ]
device = [ "{device:device.nick}", "{device:device.description}" ]
```

See [macmix.toml](./macmix.toml) for the full template syntax and per-object
overrides.

### Filters

You can hide objects from the lists based on their properties. See
[macmix.toml](./macmix.toml) for details.

## Credits & License

macmix is a fork of [wiremix](https://github.com/tsowell/wiremix) by Thomas
Sowell: it reuses wiremix's excellent terminal UI and replaces the Linux
PipeWire backend with a native macOS CoreAudio one. wiremix's interface is in
turn inspired by [ncpamixer](https://github.com/fulhax/ncpamixer) and
pavucontrol.

Licensed under MIT OR Apache-2.0, the same as the original wiremix.

Issues and pull requests are welcome!
