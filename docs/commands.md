# CLI Commands

All commands require `--config=neolink.toml` and a camera name matching the
`name` field in your config.

## RTSP Server

```bash
neolink rtsp --config=neolink.toml
```

Starts the RTSP proxy. Connect clients to `rtsp://HOST:8554/CameraName`.

## Image Capture

```bash
neolink image --config=neolink.toml --file-path=snap.jpg CameraName
```

Saves a JPEG snapshot to disk. The `.jpg` extension is added or corrected
automatically.

Some cameras do not support the SNAP command. Use `--use-stream` to capture
a frame by transcoding the video stream instead.

## Battery

```bash
neolink battery --config=neolink.toml CameraName
```

Prints XML-formatted battery status to stdout.

## PIR

```bash
neolink pir --config=neolink.toml CameraName [on|off]
```

Enables or disables the PIR sensor.

## Reboot

```bash
neolink reboot --config=neolink.toml CameraName
```

Reboots the camera.

## Status LED

```bash
neolink status-light --config=neolink.toml CameraName [on|off]
```

Toggles the camera's status LED.

## Talk

Send audio to the camera speaker.

From a file (ADPCM encoded):

```bash
neolink talk --config=neolink.toml \
  --adpcm-file=data.adpc \
  --sample-rate=16000 \
  --block-size=512 \
  CameraName
```

From microphone (uses
[GStreamer autoaudiosrc](https://gstreamer.freedesktop.org/documentation/autodetect/autoaudiosrc.html)):

```bash
neolink talk --config=neolink.toml --microphone CameraName
```

## PTZ

Control pan, tilt, and zoom:

```bash
# Move with speed 32 (not all cameras support speed)
neolink ptz --config=neolink.toml CameraName control 32 [left|right|up|down|in|out]

# List preset positions
neolink ptz --config=neolink.toml CameraName preset

# Move to preset ID 0
neolink ptz --config=neolink.toml CameraName preset 0

# Save current position as preset ID 0 with name "Driveway"
neolink ptz --config=neolink.toml CameraName assign 0 Driveway

# Zoom to 2.5x (1.0 = normal)
neolink ptz --config=neolink.toml CameraName zoom 2.5
```

## Encoding

View or modify per-stream video encoding settings (bitrate, fps, rate control,
encoder profile):

```bash
neolink encoding --config=neolink.toml CameraName
```

## Disk Management

List and manage SD cards, replay recorded video, and search alarm events:

```bash
neolink disk --config=neolink.toml CameraName
```
