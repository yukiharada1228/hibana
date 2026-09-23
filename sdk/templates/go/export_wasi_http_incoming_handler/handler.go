package export_wasi_http_incoming_handler

import (
	"encoding/json"
	"os"
	"strings"

	. "example.com/hibana-app/bindings/wasi_http_types"
	"example.com/hibana-app/bindings/wasi_io_streams"
	. "go.bytecodealliance.org/pkg/wit/types"
)

// This is a WASI HTTP entry point, not a listening TCP server.
func Handle(request *IncomingRequest, responseOut *ResponseOutparam) {
	defer request.Drop()
	path := "/"
	if value := request.PathWithQuery(); value.IsSome() {
		path = strings.SplitN(value.Some(), "?", 2)[0]
	}
	status, contentType := uint16(200), "application/json"
	var message []byte
	if path == "/echo" && request.Method().Tag() == MakeMethodPost().Tag() {
		contentType = "application/octet-stream"
		var ok bool
		message, ok = readBody(request)
		if !ok {
			status, contentType, message = 400, "text/plain", []byte("Invalid request body")
		}
	} else if path == "/" {
		greeting := os.Getenv("GREETING")
		if greeting == "" {
			greeting = "Hello Hibana"
		}
		message, _ = json.Marshal(map[string]string{"message": greeting})
	} else {
		status, contentType, message = 404, "text/plain", []byte("Not found")
	}
	headers := MakeFields()
	headers.Set("content-type", [][]byte{[]byte(contentType)})
	response := MakeOutgoingResponse(headers)
	response.SetStatusCode(status)
	body := response.Body().Ok()
	ResponseOutparamSet(responseOut, Ok[*OutgoingResponse, ErrorCode](response))
	stream := body.Write().Ok()
	if request.Method().Tag() != MakeMethodHead().Tag() {
		for len(message) > 0 {
			n := min(len(message), 4096)
			if stream.BlockingWriteAndFlush(message[:n]).IsErr() {
				stream.Drop()
				body.Drop()
				return
			}
			message = message[n:]
		}
	}
	stream.Drop()
	OutgoingBodyFinish(body, None[*Fields]())
}

func readBody(request *IncomingRequest) ([]byte, bool) {
	result := request.Consume()
	if result.IsErr() {
		return nil, false
	}
	body := result.Ok()
	streamResult := body.Stream()
	if streamResult.IsErr() {
		body.Drop()
		return nil, false
	}
	stream := streamResult.Ok()
	defer func() { stream.Drop(); IncomingBodyFinish(body).Drop() }()
	var bytes []byte
	for {
		chunk := stream.BlockingRead(4096)
		if chunk.IsErr() {
			return bytes, chunk.Err().Tag() == wasi_io_streams.StreamErrorClosed
		}
		if len(bytes)+len(chunk.Ok()) > 1024*1024 {
			return nil, false
		}
		bytes = append(bytes, chunk.Ok()...)
	}
}
