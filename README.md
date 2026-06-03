# RustyVNC

RustyVNC is a standalone HVNC red team tool with a Rust Windows client and a Go relay/viewer server.

The client is Rust and owns the Windows desktop, capture, JPEG encoding, application launch, and input dispatch logic. The server is Go and owns HTTPS/WSS transport, frame validation, viewer fan-out, and the minimal browser UI.

## Layout

```text
client/   Rust Windows client.
server/   Go relay and browser viewer for the Linux operator host.
docs/     Wire protocol notes.
```

## Quick Start

Start the server on the operator host:

```sh
cd server
go run . -addr 127.0.0.1:7070
```

The server is HTTPS-only. If `-cert` and `-key` are not supplied, it generates an ephemeral self-signed certificate for the current run.

Open the viewer:

```text
https://127.0.0.1:7070/
```

Build the Windows client from Linux with the MinGW target:

```sh
cd client
cargo build --release --target x86_64-pc-windows-gnu
```

Run the client from a logged-on Windows desktop session:

```powershell
rustyvnc-client.exe --server https://SERVER_IP:7070 --debug
```

With the generated self-signed certificate, add `--accept-invalid-certs` or install a trusted certificate and run the server with `-cert` and `-key`.

The client must run in an interactive user session. Session 0 is rejected because it produces misleading black-frame behavior rather than a real desktop capture.

## How It Works

The Windows client takes an `https://` server URL and tries to open a secure WebSocket connection first. If that path is not available, it automatically falls back to HTTPS polling. Both paths stay encrypted; cleartext URLs are rejected.

The browser viewer is served from the same Go server. When the operator opens the HTTPS page, the viewer connects back over WSS and receives frames from the Windows client.

The client sends JPEG desktop frames to the server. The viewer sends mouse, keyboard, and launch actions back through the server to the client. Wire-format details live in `docs/protocol.md`.

## Notes

Only one client is supported in the first standalone server. This keeps the extraction small and makes the session lifecycle obvious. Multi-client routing can be added after the core client/server behavior is stable.

Bind the server to `127.0.0.1` by default. If you bind to a non-local interface, provide a trusted TLS certificate, use the optional `-token` flag, and keep it inside an isolated network.

*Footnote: RustyVNC is intended for authorized research and defense analyst purposes.*
