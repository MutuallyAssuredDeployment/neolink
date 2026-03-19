# MQTT

Neolink can publish camera status and receive control commands over MQTT,
with optional Home Assistant discovery.

## Configuration

```toml
bind = "0.0.0.0"

[mqtt]
broker_addr = "127.0.0.1"
port = 1883
credentials = ["username", "password"]

[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"
```

Start with MQTT + RTSP:

```bash
neolink mqtt-rtsp --config=neolink.toml
```

Or MQTT only:

```bash
neolink mqtt --config=neolink.toml
```

## Global Messages

Prefixed with `neolink/`:

| Topic | Description |
|---|---|
| `/status` | Connection status: `connected` or `offline` (LastWill) |
| `/config` | Publish here to **temporarily** alter the live configuration |
| `/config/status` | Result of a `/config` publish: errors or `Ok(())` |

## Per-Camera Messages

Prefixed with `neolink/{CAMERA_NAME}`.

### Control Messages

| Topic | Payload | Description |
|---|---|---|
| `/control/led` | `on\|off` | Toggle status LED |
| `/control/ir` | `on\|off\|auto` | IR lights |
| `/control/reboot` | *(empty)* | Reboot camera |
| `/control/ptz` | `up\|down\|left\|right\|in\|out [amount]` | PTZ movement (default amount: 32.0) |
| `/control/ptz/preset` | `[id]` | Move to PTZ preset |
| `/control/ptz/assign` | `[id] [name]` | Save current position as preset |
| `/control/zoom` | `[amount]` | Zoom factor (1.0 = normal, 3.5 = 3.5x) |
| `/control/pir` | `on\|off` | Toggle PIR sensor |
| `/control/floodlight` | `on\|off` | Toggle floodlight |
| `/control/floodlight_tasks` | `on\|off` | Toggle automatic floodlight triggers (motion, night) |
| `/control/wakeup` | `[mins]` | Force wakeup for given minutes (for `idle_disconnect` cameras) |
| `/control/siren` | `on` | Activate siren (no "off" signal exists) |

### Status Messages

| Topic | Description |
|---|---|
| `/status` | `disconnected` when camera goes offline |
| `/status/battery` | XML battery status (reply to `/query/battery`) |
| `/status/battery_level` | Battery % (requires `enable_battery = true`) |
| `/status/pir` | XML PIR status (reply to `/query/pir`) |
| `/status/motion` | `on` for motion, `off` for still (requires `enable_motion = true`) |
| `/status/ptz/preset` | XML PTZ presets (reply to `/query/ptz/preset`) |
| `/status/preview` | Base64 JPEG, updated every 2s (requires `enable_preview = true`) |
| `/status/floodlight_tasks` | Floodlight task status, updated every 2s |

### Query Messages

| Topic | Description |
|---|---|
| `/query/battery` | Request battery level report |
| `/query/pir` | Request PIR status report |
| `/query/ptz/preset` | Request PTZ preset list |
| `/query/preview` | Request immediate preview image |

## Controlling RTSP from MQTT

When started with `mqtt-rtsp`, publish to `/neolink/config` to change RTSP
settings at runtime:

```toml
# Change available users
[[users]]
name = "me"
pass = "mepass"

# Change permitted users on a camera
[[cameras]]
permitted_users = ["me"]

# Change stream (set to "None" to disable)
[[cameras]]
stream = "Main"

# Disable a camera entirely
[[cameras]]
enabled = false
```

## Per-Camera MQTT Options

Disable features to conserve battery or reduce traffic:

```toml
[cameras.mqtt]
enable_motion = false         # motion detection (passive, low battery drain)
enable_light = false          # floodlight status (passive, low battery drain)
enable_battery = false        # battery updates in /status/battery_level
enable_preview = false        # preview image in /status/preview
enable_floodlight = false     # floodlight task status
battery_update = 2000         # ms between battery updates
preview_update = 2000         # ms between preview updates
floodlight_update = 2000      # ms between floodlight task updates
```

## Home Assistant Discovery

[MQTT Discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery)
is partially supported. Discovery is opt-in and features must be manually
specified:

```toml
[cameras.mqtt.discovery]
topic = "homeassistant"
features = ["floodlight", "camera", "led", "ir", "motion", "reboot", "pt", "battery", "siren"]
```

| Feature | Description |
|---|---|
| `floodlight` | Light control |
| `camera` | Preview image (updated ~every 0.5s over MQTT, not RTSP). Not all cameras support the snapshot command. |
| `led` | Status LED on/off switch |
| `ir` | IR light on/off/auto selector |
| `motion` | Motion detection binary sensor |
| `reboot` | Reboot button |
| `pt` | Pan and tilt buttons |
| `battery` | Battery level sensor |
| `siren` | Siren button |
