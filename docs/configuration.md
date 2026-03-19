# Configuration Reference

Neolink is configured with a TOML file. See
[sample_config.toml](https://github.com/janost/neolink/blob/master/sample_config.toml)
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

# Connection: use address OR uid (not both)
address = "192.168.1.10:9000"    # direct IP connection
uid = "ABCDEF0123456789"        # UID-based discovery

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
update_time = false              # force camera clock sync on connect

# NVR channel (for multi-camera NVRs)
channel_id = 0                   # 0-indexed camera on NVR
```

## Discovery

When connecting by UID, the camera IP is discovered with these methods. Each
method implicitly enables all prior methods.

| Method | How it works | Requirements |
|---|---|---|
| `local` | UDP broadcast on local network | Network supports broadcasts |
| `remote` | Ask Reolink servers for camera IP, connect directly | Route to camera + camera can reach Reolink |
| `map` | Register our IP with Reolink, camera connects to us | Our IP and Reolink reachable from camera |
| `relay` | Reolink relays all traffic | Both sides can reach Reolink |
| `cellular` | Alias for map + relay only | For cellular cameras (skips local/remote) |

```toml
discovery = "local"    # default; use "cellular" for cellular cameras
```

If you know the camera's IP address, set `address` directly and skip discovery
entirely.

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
