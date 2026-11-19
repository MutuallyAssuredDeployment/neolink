# Using the dissector

The dissector can be used with `wireshark` and `tshark`. It also supports **exporting replay streams** (SD card playback / download) from a pcap to a file.

## Exporting replay video from a pcap

1. Open your pcap in Wireshark (with the Baichuan dissector installed). Replay packets (message IDs 5, 8, 143, 381), including empty completion messages, are collected automatically during dissection.
2. **Tools → Export Baichuan Replay to File** — reassembles streams per connection/request, keeps InfoV1/V2 and writes `replay_1.bin`, `replay_2.bin`, … (or `.mp4` if the payload contains `ftyp` at offsets 4-7). An unknown 32-byte prefix is removed only before a recognized stream.
3. Set **Edit → Preferences → Protocols → Baichuan → Replay export directory** to choose where files are saved (empty = current directory).

Exported files are raw replay payloads (often MP4). `encryptLen`/`encryptPos` select the AES-128/CFB region; continuation bytes remain unchanged. Set the decryption key to decrypt XML and these media regions. Undecodable sections are skipped and reported as `baichuan.replay.error`.

331 with section metadata or remaining `FileCount` starts another recording section; legacy final 331 and final 300 end reception. Sections are exported separately as `replay_1_section_1.bin`, …, preserving overlapping recordings. Missing start or completion uses `_partial`; packet loss and media-frame integrity are not validated. See [the download sequence](protocol.md#recording-download-id-143).

Filter fields: `baichuan.replay.event`, `baichuan.replay.file_count`, `baichuan.replay.section_metadata`, `baichuan.replay.media_type`, `baichuan.replay.error`. Media types describe recognized message prefixes, not every frame in a fragmented stream. ID 144 is labelled as download stop for the observed Cam Argus 2/Cam Argus 4 flow.

## Dissector tests

Tests use synthetic messages, Wireshark API doubles and AES vectors; no PCAP or camera is required.

```sh
python -m pip install -r dissector/tests/requirements.txt
python dissector/tests/test_replay.py
```

---

For **AES decryption** of Baichuan payloads the dissector uses, in order:

1. **Wireshark's built-in Gcrypt Lua API** (Wireshark 4.x) – no extra install. The dissector uses this when available.
2. **Optional fallback: [`luagcrypt`](https://github.com/Lekensteyn/luagcrypt)** – an older, unmaintained C module; only needed if your Wireshark build does not expose GcryptCipher to Lua. Not packaged in most Linux distributions; build from source. Instructions below are for Debian and its derivatives (tested on Debian 12 Bookworm amd64).

Without either, the dissector still decodes BC headers and XOR-encrypted XML; only AES-decrypted payloads are skipped.

## Build `luagcrypt.so` (optional fallback)
```
sudo apt install lua5.2 liblua5.2-dev libgcrypt20-dev libgpg-error-dev
git clone https://github.com/Lekensteyn/luagcrypt.git
cd luagcrypt
make LUA_DIR=/usr
```
## Install `luagcrypt.so`
The shared object library should be copied to `/usr/local/lib/lua/5.2/`
```
mkdir --parents /usr/local/lib/lua/5.2/
cp luagcrypt.so /usr/local/lib/lua/5.2/
```
Additionally, the system where the dissector is used needs these packages installing (if not already present): `libgcrypt20 libgpg-error0`
## Install `baichuan.lua`
Copy the dissector to the host where `wireshark` (or `tshark`) will be used to analyse the captured packets:
```
mkdir --parents $HOME/.local/lib/wireshark/plugins/
cp baichuan.lua $HOME/.local/lib/wireshark/plugins/
```

