INSTALL_PROGRAM = install -D -m 0755
install_loc = ${DESTDIR}/usr/bin

all: build

distclean: clean

binary: binary-arch binary-indep

binary-arch: build-arch

binary-indep: build-indep

build-arch: build

build-indep: build

build:
	cargo build --release

clean:
	cargo clean

install:
	mkdir -p $(install_loc)
	$(INSTALL_PROGRAM) "./target/release/regolith-displayd" "$(install_loc)/"
