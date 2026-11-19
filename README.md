# Neolink

[![Docker GHCR](https://github.com/mhhoeper/neolink/actions/workflows/docker-ghcr.yml/badge.svg)](https://github.com/mhhoeper/neolink/actions/workflows/docker-ghcr.yml)
[![dependency status](https://deps.rs/repo/github/mhhoeper/neolink/status.svg)](https://deps.rs/repo/github/mhhoeper/neolink)

Neolink is a small program that acts as a proxy between Reolink IP cameras and
normal RTSP clients.
Certain cameras, such as the Reolink B800, do not implement ONVIF or RTSP, but
instead use a proprietary "Baichuan" protocol only compatible with their apps
and NVRs (any camera that uses "port 9000" will likely be using this protocol).
Neolink allows you to use NVR software such as Blue Iris, Frigate, or Shinobi to
receive video from these cameras instead.
The Reolink NVR is not required, and the cameras are unmodified.
Your NVR software connects to Neolink, which forwards the video stream from the
camera.

The Neolink project is not affiliated with Reolink in any way; everything it
does has been reverse engineered.

## This Fork

This is a maintained fork of
[MutuallyAssuredDeployment/neolink](https://github.com/MutuallyAssuredDeployment/neolink)
which itself is a fork of
[QuantumEntangledAndy/neolink](https://github.com/QuantumEntangledAndy/neolink),
which itself was a fork of
[thirtythreeforty/neolink](https://github.com/thirtythreeforty/neolink).

## Installation

Docker is the preferred way to run Neolink. Binary releases are not currently
provided.

### Docker (recommended)

Images are published to GHCR on every push to `master` and on tags:

```bash
docker pull ghcr.io/mhhoeper/neolink:dev      # latest master
docker pull ghcr.io/mhhoeper/neolink:latest   # latest version-tagged image
```

Each image's GHCR description includes the full build commit and a link to that
exact source revision. Images are labeled `AGPL-3.0-or-later` and include the
license text at `/usr/share/doc/neolink/LICENSE`.

Version tags publish versioned Docker images and update `latest`. Distribution
uses GHCR; the workflows do not create GitHub releases or binary downloads.

Run with:

```bash
docker run \
  --network host \
  --volume=$PWD/neolink.toml:/etc/neolink.toml \
  ghcr.io/mhhoeper/neolink
```

> `--network host` is only needed for local broadcast discovery. If you connect
> via IP address, normal bridge mode works fine. macOS does not support
> `--network host`.

**Environment variables:**

| Variable | Default | Description |
|---|---|---|
| `NEO_LINK_MODE` | `rtsp` | Mode: `rtsp`, `mqtt`, or `mqtt-rtsp` |
| `NEO_LINK_PORT` | `8554` | RTSP listen port |

### Building from source

Requires Rust and GStreamer development libraries:

```bash
# Ubuntu/Debian
sudo apt install \
  build-essential libssl-dev ca-certificates \
  libgstrtspserver-1.0-dev libgstreamer1.0-dev \
  libglib2.0-dev protobuf-compiler

cargo build --release
```

## Documentation

Detailed docs are in the [`docs/`](docs/) directory:

- [Configuration Reference](docs/configuration.md) — all config options, discovery, TLS, users, pause/idle
- [MQTT](docs/mqtt.md) — full message reference, Home Assistant discovery, runtime control
- [CLI Commands](docs/commands.md) — image capture, PTZ, talk, encoding, disk management
- [Setting Up with Blue Iris](docs/Setting%20Up%20Neolink%20For%20Use%20With%20Blue%20Iris.md)
- [Unix Service Setup](docs/unix_service.md)
- [Unix Build Setup](docs/unix_setup.md)

## Configuration

Create a `neolink.toml` file. See
[sample_config.toml](https://github.com/MutuallyAssuredDeployment/neolink/blob/master/sample_config.toml)
for all options.

Minimal example:

```toml
bind = "0.0.0.0"

[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"

[[cameras]]
name = "Camera02"
username = "admin"
password = "password"
address = "192.168.1.10:9000"
```

Start the RTSP server:

```bash
neolink rtsp --config=neolink.toml
```

Connect your NVR or RTSP client to:

```
rtsp://HOST:8554/CameraName
```

Where "CameraName" matches the `name` field in your config.

### Per-camera options

```toml
[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
address = "192.168.1.10:9000"
stream = "mainStream"          # "mainStream", "subStream", or "both" (default)
enable_audio = true            # disable to save CPU (default: true)
enable_low_latency = false     # reduce stream delay (default: false)
discovery = "local"            # "none", "local", "remote", "map", "relay", "cellular" (default: "relay")
idle_disconnect = false        # disconnect when idle to save battery
debug = false                  # dump raw XML from camera
```

### Discovery

When connecting by UID, the camera IP is discovered with these methods (each
implicitly enables prior methods):

1. **local** — UDP broadcast on local network
2. **remote** — Ask Reolink servers for the IP, then connect directly
3. **map** — Register our IP with Reolink, camera connects to us
4. **relay** — Reolink relays the connection (neither side needs direct access)

Use `discovery = "cellular"` for cellular cameras (skips local/remote).

The default is `discovery = "relay"`. Use `discovery = "local"` with a UID to
connect on the local network without contacting Reolink servers.

With a known IP, Neolink first probes TCP (normally port 9000). Setting
`discovery = "none"` disables UID-based UDP discovery and connection setup, so
it requires a reachable Baichuan TCP service. The TCP probe still runs. For UDP
cameras, retain the UID and an appropriate discovery mode. Supplying `address`
alone does not disable broadcasts when `local` discovery is used.

### MQTT

```toml
[mqtt]
broker_addr = "127.0.0.1"
port = 1883
credentials = ["username", "password"]
```

Start with MQTT + RTSP:

```bash
neolink mqtt-rtsp --config=neolink.toml
```

Or MQTT only:

```bash
neolink mqtt --config=neolink.toml
```

#### Messages

All messages are prefixed with `neolink/` or `neolink/{CAMERA_NAME}/`.

**Control** (`/control/...`): `led`, `ir`, `reboot`, `ptz`, `zoom`, `pir`,
`floodlight`, `floodlight_tasks`, `wakeup`, `siren`

**Status** (`/status/...`): `battery`, `battery_level`, `motion`, `pir`,
`ptz/preset`, `preview`, `floodlight_tasks`

**Query** (`/query/...`): `battery`, `pir`, `ptz/preset`, `preview`

See the
[sample config](https://github.com/MutuallyAssuredDeployment/neolink/blob/master/sample_config.toml)
for per-camera MQTT options (`enable_motion`, `enable_preview`,
`enable_battery`, etc.).

#### Home Assistant Discovery

```toml
[cameras.mqtt.discovery]
topic = "homeassistant"
features = ["floodlight", "camera", "led", "ir", "motion", "reboot", "pt", "battery", "siren"]
```

### Pause & Idle Disconnect

```toml
[cameras.pause]
on_motion = true    # pause when no motion
on_client = true    # pause when no RTSP client
timeout = 2.1       # seconds after motion stops before pausing
```

Add `idle_disconnect = true` to fully disconnect battery cameras when idle.

### Other Commands

```bash
neolink image    --config=neolink.toml --file-path=snap.jpg CameraName
neolink battery  --config=neolink.toml CameraName
neolink pir      --config=neolink.toml CameraName [on|off]
neolink reboot   --config=neolink.toml CameraName
neolink status-light --config=neolink.toml CameraName [on|off]
neolink ptz      --config=neolink.toml CameraName control 32 [left|right|up|down|in|out]
neolink ptz      --config=neolink.toml CameraName preset [id]
neolink ptz      --config=neolink.toml CameraName zoom 2.5
neolink talk     --config=neolink.toml --microphone CameraName
neolink encoding --config=neolink.toml CameraName
neolink disk     --config=neolink.toml CameraName
```

## License

Neolink is free software, released under the GNU Affero General Public License
v3 or later.

## Donations

The original upstream project was created by
[@thirtythreeforty](https://github.com/thirtythreeforty) and maintained by
[@QuantumEntangledAndy](https://github.com/QuantumEntangledAndy) and [@MutuallyAssuredDeployment](https://github.com/MutuallyAssuredDeployment). If you'd like
to support my work:

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/marc367548)
