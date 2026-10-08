.PHONY: build test install bench-demo
build:      ; cargo build --release
test:       ; cargo test
install:    ; cargo install --path .
# 100 simulated agent sessions on one server, through Carpool and directly
bench-demo: build
	./target/release/carpool bench -n 100 -k 10 --tool get_current_time --arguments '{"timezone":"UTC"}' -- uvx mcp-server-time
	./target/release/carpool bench -n 100 -k 10 --direct --tool get_current_time --arguments '{"timezone":"UTC"}' -- uvx mcp-server-time
