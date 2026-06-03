# RustyVNC Protocol

RustyVNC uses two WebSocket endpoints.

`/ws/client` is used by the Rust Windows client.

`/ws/viewer` is used by the browser viewer.

The server is a relay. It validates frame size and dimensions, stores the latest frame, and fans frames out to connected viewers.

## Client Text Events

The client sends JSON text events:

```json
{ "type": "hello", "client_id": "...", "platform": "windows", "user": "Administrator", "host": "CRM-SRV" }
{ "type": "started", "client_id": "...", "conn_id": "..." }
{ "type": "stopped", "client_id": "..." }
{ "type": "error", "client_id": "...", "message": "..." }
```

The server sends JSON text commands:

```json
{ "type": "start", "quality": 70 }
{ "type": "stop" }
{ "type": "ping" }
```

## Client Binary Frames

The client sends HVNC frame packets:

```text
0x00
```

No frame change.

```text
0x01 <width:u32le> <height:u32le> <jpeg bytes>
```

JPEG frame. The server rejects zero dimensions, dimensions above `8192x8192`, and JPEG bodies above 5 MB.

## Viewer Text Commands

The viewer sends JSON commands:

```json
{ "type": "status" }
{ "type": "start", "quality": 70 }
{ "type": "stop" }
{ "type": "launch", "action": "cmd" }
{ "type": "input", "msg": 512, "wparam": 0, "lparam": 12345 }
```

Supported launch actions are `explorer`, `run`, `chrome`, `edge`, `brave`, `firefox`, `powershell`, and `cmd`.

## Viewer Binary Frames

The server sends binary viewer frames:

```text
16 bytes  client UUID
4 bytes   width, little endian
4 bytes   height, little endian
n bytes   JPEG frame
```

The fixed header keeps the browser canvas logic simple and lets viewers ignore frames from other clients if multi-client routing is added later.
