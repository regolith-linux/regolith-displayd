build:
	mkdir -p debian/tmp_files/.cargo
	CARGO_HOME=debian/tmp_files/.cargo cargo build --release

test:
	CARGO_HOME=debian/tmp_files/.cargo cargo test --release

clean:
	cargo clean

