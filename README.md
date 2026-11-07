# scrcpyt

Mirror and control an Android device inside your terminal.

scrcpyt runs the stock [scrcpy](https://github.com/Genymobile/scrcpy) server on
the device, decodes the video with FFmpeg and draws it with the
[kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).
Mouse and keyboard input are forwarded to the device, and the clipboard is
synchronized both ways.

## Requirements

- A terminal supporting the kitty graphics protocol, SGR-Pixels mouse
  reporting (`?1016`) and OSC 52, e.g. Ghostty, kitty or Rex
- [scrcpy](https://github.com/Genymobile/scrcpy) installed: its server jar is
  reused, and its version must be the one the jar was built for
- `adb` in `PATH` (or set `ADB`)
- FFmpeg libraries and `pkg-config` to build

On macOS:

```sh
brew install scrcpy ffmpeg pkg-config
```

## Build

```sh
cargo build --release
```

## Usage

```sh
scrcpyt            # the only connected device
scrcpyt -d         # the USB device
scrcpyt -e         # the emulator
scrcpyt -s SERIAL  # a device listed by `adb devices`
```

| Option | |
| --- | --- |
| `-m, --max-size` | Limit the video's largest dimension (default: terminal size in pixels) |
| `--max-fps` | Limit the frame rate |

### Controls

| Input | Action |
| --- | --- |
| Left click / drag | Touch |
| Right click | Back (turns the screen on if off) |
| Middle click | Home |
| Wheel | Scroll |
| Paste | Paste through the device clipboard |
| `Alt+h` / `Alt+b` / `Alt+s` | Home / Back / App switch |
| `Alt+m` / `Alt+p` | Menu / Power |
| `Alt+n` / `Alt+Shift+n` | Expand / collapse the notification panel |
| `Alt+o` / `Alt+Shift+o` | Turn the device screen off / on, mirroring continues |
| `Alt+r` | Rotate |
| `Alt+Up` / `Alt+Down` | Volume up / down |
| `Ctrl+q` | Quit |

Other keys, including `Ctrl+c`, are sent to the device.

## Environment

| Variable | |
| --- | --- |
| `ADB` | adb binary |
| `SCRCPY_SERVER_PATH` | scrcpy server jar, if not found next to `scrcpy` |
| `SCRCPYT_LOG` | Debug log file |

## Troubleshooting

Xiaomi devices reject input until *USB debugging (Security settings)* is
enabled in the developer options. The server errors are printed on exit.

## License

Apache-2.0
