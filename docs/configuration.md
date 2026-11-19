# Configuration Reference

Neolink is configured with a TOML file. See
[sample_config.toml](https://github.com/MutuallyAssuredDeployment/neolink/blob/master/sample_config.toml)
for a complete example.

## Global Settings

```toml
bind = "0.0.0.0"           # listen address (0.0.0.0 = all interfaces)
bind_port = 8554            # RTSP port (default: 8554)
```

### TLS

```toml
certificate = "/path/to/cert-and-key.pem"   # enables rtsps://
tls_client_auth = "none"                    # none | requested | required
```

### Users

Password-protect RTSP streams:

```toml
[[users]]
name = "me"
pass = "mepass"
```

Access with `rtsp://me:mepass@HOST:8554/CameraName`. If no `[[users]]` are
defined, anyone can connect without authentication.

## Camera Settings

```toml
[[cameras]]
name = "Camera01"
username = "admin"
password = "password"

# Connection: provide address, uid, or both
address = "192.168.1.10:9000"    # known IP; TCP is tried first
uid = "ABCDEF0123456789"         # required for UDP discovery/connection

# Stream selection
stream = "both"                  # "mainStream", "subStream", or "both" (default)
permitted_users = ["me"]         # restrict to specific users (default: all)
enabled = true                   # disable without removing from config

# Audio & latency
enable_audio = true              # set false to save CPU (default: true)
enable_low_latency = false       # reduce stream delay (default: false)

# Diagnostics
debug = false                    # dump raw XML from camera
print_format = "None"            # "None", "Human", or "Xml" for status messages
update_time = false              # force camera clock sync on connect (uses local wall-clock time)

# NVR channel (for multi-camera NVRs)
channel_id = 0                   # 0-indexed camera on NVR
```

## Discovery

The `discovery` setting controls UID-based UDP discovery and connection setup.
The default is `relay`. The modes `local`, `remote`, `map`, and `relay` are
cumulative: each permits the preceding methods as well. `none` disables them
all, while `cellular` permits only map and relay.

| Method | How it works | Requirements |
|---|---|---|
| `none` | Disable all UID-based UDP discovery and connection setup | Known address with a reachable Baichuan TCP service |
| `local` | UDP broadcasts and any supplied camera address; no Reolink servers | UID and local network access |
| `remote` | Ask Reolink servers for camera IP, connect directly | Route to camera + camera can reach Reolink |
| `map` | Register our IP with Reolink, camera connects to us | Our IP and Reolink reachable from camera |
| `relay` | Permit local, remote, map, and relayed traffic (default) | Both sides can reach Reolink for relay |
| `cellular` | Alias for map + relay only | For cellular cameras (skips local/remote) |

```toml
discovery = "relay"    # default; use "cellular" for cellular cameras
```

For local UDP connections without contacting Reolink servers, the UID is enough;
an IP address is optional:

```toml
uid = "ABCDEF0123456789"
discovery = "local"
```

If `address` is supplied, Neolink first probes the camera's Baichuan TCP service
(normally port 9000). A successful probe allows a TCP connection. Otherwise,
Neolink can attempt UDP if a UID and a suitable discovery mode are configured.
For `local`, the supplied address is tried in addition to the broadcasts;
specifying an IP address does not disable broadcast discovery.

To disable UID-based UDP discovery and connection setup, use:

```toml
address = "192.168.1.10:9000"
discovery = "none"
```

`none` still performs the TCP probe. It cannot connect to a camera through UDP,
even when its IP is known. If TCP is unavailable, the connection fails. Ping
reachability alone does not establish that the TCP service is available.

## Pause & Idle Disconnect

Pause the stream when not needed to reduce bandwidth and camera load:

```toml
[cameras.pause]
on_motion = true     # pause when no motion detected
on_client = true     # pause when no RTSP client connected
timeout = 2.1        # seconds to wait after motion stops before pausing
```

### Battery Camera Idle Disconnect

For battery cameras, fully disconnect when idle to conserve power:

```toml
[[cameras]]
idle_disconnect = true    # disconnect 30s after last activity
```

Activity includes: active streams, motion detection, or MQTT commands.

The camera reconnects when a new RTSP client connects or an MQTT command is
received.

> **Note:** Push notification wakeup is no longer available (Google removed the
> required APIs).

## MQTT

See [mqtt.md](mqtt.md) for full MQTT configuration and message reference.
