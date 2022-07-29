INSTALL_PROGRAM = install -D -m 0755
INSTALL_DATA = install -D -m 0644 
install_loc = ${DESTDIR}/usr/bin
service_loc = ${DESTDIR}/usr/lib/systemd/user/

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
	sudo $(INSTALL_PROGRAM) "./target/release/regolith-displayd" "$(install_loc)/"
	sudo $(INSTALL_DATA) "./regolith-displayd.service" "$(service_loc)/regolith-displayd.service"
