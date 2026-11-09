.PHONY: onboard setup build test run demo

onboard:
	./scripts/onboard.sh

setup:
	./scripts/bootstrap.sh

build: setup
	cargo build --release

test: setup
	cargo test

run: build
	./target/release/topterm -t

demo: build
	python3 tools/capture_frame.py 140 36 /tmp/frame.ans ./target/release/topterm --fake-data --demo-frame
	python3 tools/ansi2svg.py /tmp/frame.ans assets/demo.svg "topterm — ttys004 leaking"
