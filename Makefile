INSTALL_PROGRAM = install -D -m 0755
INSTALL_DATA = install -D -m 0644 

all: build

distclean: clean

binary: binary-arch binary-indep

binary-arch: build-arch

binary-indep: build-indep

build-arch: build

build-indep: build

build:
	mkdir -p debian/tmp_files/.cargo
	CARGO_HOME=debian/tmp_files/.cargo cargo build --release

clean:
	cargo clean

