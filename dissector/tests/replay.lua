-- No cameras/PCAPs required. Test the actual dissector through a small Wireshark API double.
bit = {
  band = function(a, b) return (a & b) & 0xffffffff end,
  bor = function(a, b) return (a | b) & 0xffffffff end,
  bxor = function(a, b) return (a ~ b) & 0xffffffff end,
  bnot = function(a) return (~a) & 0xffffffff end,
  lshift = function(a, n) return (a << n) & 0xffffffff end,
  rshift = function(a, n) return (a & 0xffffffff) >> n end,
}
base = { DEC = 1, HEX = 2, ASCII = 3, NONE = 4 }
local protocol, current_info, export_menu, files, fields
Proto = function(name)
  protocol = { name = name, prefs = {}, register_heuristic = function() end }
  return protocol
end
ProtoField = setmetatable({}, { __index = function()
  return function(name) return { name = name } end
end })
Pref = { string = function(_, default) return default end }
Field = { new = function(name) return function()
  if name == "tcp.stream" then return current_info and current_info.tcp_stream end
  if name == "tcp.seq_raw" then return current_info and current_info.tcp_sequence end
end end }
Dissector = { get = function() return { call = function() end } end }
DissectorTable = { get = function() return { add = function() end } end }
register_menu = function(_, callback) export_menu = callback end
MENU_TOOLS_UNSORTED = 1
gui = { message_box = function() end }

local bytes, methods = {}, {}
methods.__index = methods
methods.__call = function(self, offset, length)
  offset = offset or 0
  length = length or (#self.data - offset)
  assert(offset >= 0 and length >= 0 and offset + length <= #self.data, "out-of-bounds buffer access")
  return bytes(self.data:sub(offset + 1, offset + length))
end
bytes = function(data) return setmetatable({ data = data }, methods) end
function methods:len() return #self.data end
function methods:raw(offset, length) return offset and self(offset, length).data or self.data end
function methods:string() return self.data end
function methods:bytes() return self end
function methods:tvb() return self end
function methods:le_uint()
  local value = 0
  for i = #self.data, 1, -1 do value = value * 256 + self.data:byte(i) end
  return value
end
function methods:get_index(i) return self.data:byte(i + 1) end
function methods:set_index(i, value)
  self.data = self.data:sub(1, i) .. string.char(value) .. self.data:sub(i + 2)
end
function methods:set_size(size) self.data = self.data:sub(1, size) .. string.rep("\0", math.max(0, size - #self.data)) end
methods.__concat = function(a, b) return bytes(a.data .. b.data) end
ByteArray = { new = function(data, raw)
  if not data then return bytes("") end
  if raw then return bytes(data) end
  return bytes((data:gsub("%x%x", function(hex) return string.char(tonumber(hex, 16)) end)))
end }
local tree = {}
function tree:add(field, value)
  if type(field) == "table" and field.name then
    fields[#fields + 1] = { name = field.name, value = value }
  end
  return self
end
tree.add_le = tree.add
function tree:append_text() return self end
function tree:set_generated() return self end

GCRY_CIPHER_AES128, GCRY_CIPHER_MODE_CFB = 7, 3
GcryptCipher = { open = function(algorithm, mode)
  assert(algorithm == GCRY_CIPHER_AES128 and mode == GCRY_CIPHER_MODE_CFB)
  local cipher = {}
  function cipher:setkey(key) self.key = key:raw(); assert(#self.key == 16) end
  function cipher:setiv(iv) self.iv = iv:raw(); assert(#self.iv == 16) end
  function cipher:decrypt(_, data) return bytes(aes_decrypt_vector(self.key, self.iv, data:raw())) end
  return cipher
end }
local aes_backend = GcryptCipher
dofile(dissector_path)

-- File output is captured in memory; no export artifacts are written by these tests.
io.open = function(name)
  files[name] = ""
  local file = { name = name }
  function file:write(data) files[self.name] = (files[self.name] or "") .. data; return self end
  function file:close() return true end
  return file
end
local function le(value, count)
  local output = {}
  for i = 1, count do output[i] = string.char(value % 256); value = math.floor(value / 256) end
  return table.concat(output)
end
local function message(id, status, payload, extension, class, handle)
  payload, extension = payload or "", extension or ""
  return le(0x0abcdef0, 4) .. le(id, 4) .. le(#payload + #extension, 4)
    .. "\0\0" .. le(handle or 0, 2) .. le(status, 2) .. le(class or 0x6414, 2)
    .. le(#extension, 4) .. extension .. payload
end
local frame = 0
local function send(raw, camera, reply, transport, packet_id, capture_frame)
  frame = frame + 1
  camera = camera or "192.0.2.10"
  current_info = { src = reply and camera or "192.0.2.1", dst = reply and "192.0.2.1" or camera,
    src_port = reply and 9000 or 50000, dst_port = reply and 50000 or 9000,
    number = capture_frame or frame, can_desegment = transport == "udp" and 0 or 1,
    cols = {} }
  if transport ~= "udp" then current_info.tcp_stream = camera end
  if transport == "udp" then
    raw = le(0x2a87cf10, 4) .. le(42, 4) .. le(0, 4) .. le(packet_id, 4) .. le(#raw, 4) .. raw
  end
  protocol.dissector(bytes(raw), current_info, tree)
end
local xml = '<?xml version="1.0"?><body><FileInfoList><FileInfo><FileCount>2</FileCount></FileInfo></FileInfoList></body>'
local info = "1002" .. le(32, 4) .. string.rep("\0", 24)
local metadata = "10 \0" .. string.rep("\0", 28)
local video = "00dcH264" .. string.rep("V", 40)
local function start(id, camera) send(message(id or 143, 0, '<?xml version="1.0"?><body/>'), camera) end
local function data(payload, id, camera, class, status, extension, handle)
  send(message(id or 143, status or 200, payload, extension, class, handle), camera, true)
end
local function finish(id, camera, status) data("", id, camera, nil, status or 300) end
local function expect(name, value)
  export_menu()
  if files[name] ~= value then
    for filename, content in pairs(files) do print(filename, #content) end
    for _, field in ipairs(fields) do
      if field.name == "baichuan.replay.event" or field.name == "baichuan.replay.error" then print(field.name, field.value) end
    end
    error("unexpected export: " .. name)
  end
end
local function count_files()
  local count = 0; for _ in pairs(files) do count = count + 1 end; return count
end
local function has_field(name, value)
  for _, field in ipairs(fields) do if field.name == name and field.value == value then return true end end
end
local passed = 0
local function test(name, body)
  files, fields, frame = {}, {}, 0
  GcryptCipher = aes_backend
  protocol.init()
  body()
  passed = passed + 1
  print("PASS " .. name)
end

test("Cam Argus 2: one request, two sections, metadata/331 then empty/300", function()
  start(); data(xml); data(info .. video); data(metadata, nil, nil, nil, 331); data(video); finish()
  expect("replay_1_section_1.bin", info .. video)
  expect("replay_1_section_2.bin", video)
  assert(count_files() == 2)
  assert(has_field("baichuan.replay.file_count", 2))
  assert(has_field("baichuan.replay.event", "Recording section boundary (331); transfer continues"))
  assert(has_field("baichuan.replay.event", "Final completion (300)"))
end)
test("FileCount bounds empty 331 section markers", function()
  start(); data(xml); data(info); finish(nil, nil, 331); data(video); finish(nil, nil, 331)
  expect("replay_1_section_1.bin", info); expect("replay_1_section_2.bin", video)
end)
test("Legacy final 331 stays supported", function()
  start(5); data(info .. video, 5); finish(5, nil, 331)
  expect("replay_1.bin", info .. video)
end)
test("Legacy 331 retains a final media payload", function()
  start(5); data(info, 5); data(video, 5, nil, nil, 331)
  expect("replay_1.bin", info .. video)
end)
test("ID 8 and ID 381 exports remain supported", function()
  start(8); data(info, 8); finish(8)
  start(381); data(video, 381); finish(381)
  expect("replay_1.bin", info); expect("replay_2.bin", video)
end)
test("Opaque Cam Argus 4 status words do not terminate reception", function()
  start(5); data(info, 5)
  data(video, 5, nil, 0x6af5, 300, nil, 123)
  data(video, 5, nil, 0x6af5, 331, nil, 456)
  finish(5)
  expect("replay_1.bin", info .. video .. video)
end)
test("InfoV1/V2 packets are preserved", function()
  start(); data(info); finish(); expect("replay_1.bin", info)
end)
test("Unknown prefix is removed only after a positive format match across messages", function()
  local mp4 = le(0x18000000, 4) .. "ftyp" .. string.rep("M", 32)
  start(); data(string.rep("X", 32)); data(mp4:sub(1, 3)); data(mp4:sub(4)); finish()
  expect("replay_1.mp4", mp4)
end)
test("Unrecognized 32-byte data remains intact", function()
  start(); data(string.rep("X", 32)); finish(); expect("replay_1.bin", string.rep("X", 32))
end)
test("Repeated dissection does not duplicate data", function()
  start(); local raw = message(143, 200, video)
  send(raw, nil, true, nil, nil, 100); send(raw, nil, true, nil, nil, 100)
  finish(); expect("replay_1.bin", video)
end)
test("Two simultaneous cameras remain separate", function()
  start(nil, "192.0.2.10"); start(nil, "192.0.2.20")
  data(info, nil, "192.0.2.10"); data(video, nil, "192.0.2.20")
  finish(nil, "192.0.2.20"); finish(nil, "192.0.2.10")
  expect("replay_1.bin", info); expect("replay_2.bin", video)
end)
test("Repeated requests on one connection create separate downloads", function()
  start(); data(info); finish(); start(); data(video); finish()
  expect("replay_1.bin", info); expect("replay_2.bin", video)
end)
test("Different handles on one connection remain separate", function()
  send(message(143, 0, "", nil, nil, 1)); send(message(143, 0, "", nil, nil, 2))
  data(info, nil, nil, nil, nil, nil, 1); data(video, nil, nil, nil, nil, nil, 2)
  data("", nil, nil, nil, 300, nil, 2); data("", nil, nil, nil, 300, nil, 1)
  expect("replay_1.bin", info); expect("replay_2.bin", video)
end)
test("Multiple BC messages in one packet retain order and empty completion", function()
  start(); send(message(143, 200, info) .. message(143, 200, video) .. message(143, 300), nil, true)
  expect("replay_1.bin", info .. video)
end)
test("Mirrored UDP copies are deduplicated by packet ID", function()
  send(message(143, 0), nil, false, "udp", 1)
  send(message(143, 200, video), nil, true, "udp", 2)
  send(message(143, 200, video), nil, true, "udp", 2)
  send(message(143, 300), nil, true, "udp", 3)
  expect("replay_1.bin", video)
end)
test("Fragmented UDP media and its mirror copies export once", function()
  send(message(143, 0), nil, false, "udp", 1)
  local raw = message(143, 200, video)
  send(raw:sub(1, 40), nil, true, "udp", 10)
  send(raw:sub(41), nil, true, "udp", 11)
  send(raw:sub(1, 40), nil, true, "udp", 10)
  send(raw:sub(41), nil, true, "udp", 11)
  send(message(143, 300), nil, true, "udp", 12)
  expect("replay_1.bin", video)
end)
test("UDP connection/packet IDs reused by another camera do not collide", function()
  for _, camera in ipairs({ "192.0.2.10", "192.0.2.20" }) do
    send(message(143, 0), camera, false, "udp", 1)
  end
  send(message(143, 200, info), "192.0.2.10", true, "udp", 2)
  send(message(143, 200, video), "192.0.2.20", true, "udp", 2)
  send(message(143, 300), "192.0.2.10", true, "udp", 3)
  send(message(143, 300), "192.0.2.20", true, "udp", 3)
  expect("replay_1.bin", info); expect("replay_2.bin", video)
end)
test("Incomplete capture and client cancellation are marked partial", function()
  start(); data(video); send(message(144, 0)); expect("replay_1_partial.bin", video)
  assert(has_field("baichuan.replay.event", "Stop download (observed ID-143 flow)"))
end)
test("A capture missing the request is marked partial", function()
  data(info); finish(); expect("replay_1_partial.bin", info)
end)
test("AES-128/CFB decrypts XML and only the advertised media prefix", function()
  start(); protocol.prefs.key = "test-password"; _G.nonce["192.0.2.10:9000"] = "TEST_NONCE"
  data(aes_encrypt_vector(xml))
  local extension = '<?xml version="1.0"?><Extension><binaryData>1</binaryData><encryptLen>16</encryptLen></Extension>'
  local payload = aes_encrypt_vector(video:sub(1, 16)) .. video:sub(17)
  data(payload, nil, nil, nil, nil, aes_encrypt_vector(extension))
  data(info); finish()
  expect("replay_1.bin", video .. info)
  assert(has_field("baichuan.replay.file_count", 2))
end)
test("XOR control XML leaves Cam Argus 2 media unchanged", function()
  local key = { 0x1f, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0xff }
  local function xor(raw)
    local result = {}
    for i = 1, #raw do result[i] = string.char(bit.bxor(raw:byte(i), key[(i - 1) % 8 + 1])) end
    return table.concat(result)
  end
  GcryptCipher = nil; start(); data(xor(xml))
  local extension = '<?xml version="1.0"?><Extension><binaryData>1</binaryData></Extension>'
  data(info .. video, nil, nil, nil, nil, xor(extension)); finish()
  expect("replay_1.bin", info .. video)
  assert(has_field("baichuan.replay.file_count", 2))
end)
test("encryptPos preserves bytes before and after the AES region", function()
  start(); protocol.prefs.key = "test-password"; _G.nonce["192.0.2.10:9000"] = "TEST_NONCE"
  local extension = '<?xml version="1.0"?><Extension><binaryData>1</binaryData><encryptPos>8</encryptPos><encryptLen>16</encryptLen></Extension>'
  local payload = video:sub(1, 8) .. aes_encrypt_vector(video:sub(9, 24)) .. video:sub(25)
  data(payload, nil, nil, nil, nil, extension); finish()
  expect("replay_1.bin", video)
end)
test("Unavailable AES skips the affected section instead of exporting ciphertext", function()
  GcryptCipher = nil; start()
  local extension = '<?xml version="1.0"?><Extension><binaryData>1</binaryData><encryptLen>16</encryptLen></Extension>'
  data(video, nil, nil, nil, nil, extension); finish(); export_menu()
  assert(count_files() == 0)
  assert(has_field("baichuan.replay.error", "AES media decryption unavailable"))
end)
test("Invalid encrypted-region bounds are reported", function()
  start(); local extension = '<?xml version="1.0"?><Extension><binaryData>1</binaryData><encryptLen>1000</encryptLen></Extension>'
  data(video, nil, nil, nil, nil, extension); finish(); export_menu(); assert(count_files() == 0)
  assert(has_field("baichuan.replay.error", "Encrypted region exceeds payload length"))
end)
test("Capture reset clears data and nonces", function()
  start(); data(video); _G.nonce.old = "test"; protocol.init(); export_menu()
  assert(count_files() == 0 and next(_G.nonce) == nil)
end)
print(passed .. " dissector replay tests passed")
