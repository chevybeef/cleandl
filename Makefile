install:
	cargo build --release
	mkdir -p ~/.local/bin
	install -m 0755 target/release/cleandl ~/.local/bin/cleandl
