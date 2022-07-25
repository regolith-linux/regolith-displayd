INSTALL = install 
INSTALL_BIN = $(INSTALL) -D -m 0755
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
	$(INSTALL_BIN) "./target/release/regolith-displayd" "$(install_loc)/"
