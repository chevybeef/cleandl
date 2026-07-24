# Detect OS and Architecture
OS := $(shell uname -s | tr A-Z a-z)
ARCH := $(shell uname -m)

# e.g., cleandl-rs-darwin-arm64 or cleandl-rs-linux-x86_64
TARGET_NAME := cleandl-rs-$(OS)-$(ARCH)

install:
	@touch .build_number
	cargo build --release
	cp target/release/cleandl-rs ~/.local/bin/$(TARGET_NAME)
#	cargo clean
