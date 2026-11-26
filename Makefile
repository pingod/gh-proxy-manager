PREFIX ?= /usr
DESTDIR ?=
BIN = target/release/gh-proxy-manager
APP_ID = io.github.duter2016.GhProxyManager
VERSION = 3.0.0
NFPM ?= nfpm

.PHONY: all build test check install uninstall dist packages appimage clean

all: build

build:
	cargo build --release --locked

test:
	cargo test --locked

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test --locked

install: build
	install -Dm755 $(BIN) "$(DESTDIR)$(PREFIX)/bin/gh-proxy-manager"
	install -Dm644 assets/$(APP_ID).desktop \
		"$(DESTDIR)$(PREFIX)/share/applications/$(APP_ID).desktop"
	install -Dm644 assets/$(APP_ID).svg \
		"$(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg"
	install -Dm644 LICENSE "$(DESTDIR)$(PREFIX)/share/licenses/gh-proxy-manager/LICENSE"
	install -Dm644 README.md "$(DESTDIR)$(PREFIX)/share/doc/gh-proxy-manager/README.md"

uninstall:
	rm -f "$(DESTDIR)$(PREFIX)/bin/gh-proxy-manager"
	rm -f "$(DESTDIR)$(PREFIX)/share/applications/$(APP_ID).desktop"
	rm -f "$(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg"
	rm -rf "$(DESTDIR)$(PREFIX)/share/licenses/gh-proxy-manager"
	rm -rf "$(DESTDIR)$(PREFIX)/share/doc/gh-proxy-manager"

dist:
	tar --exclude=target --exclude=docs --exclude=dist --transform 's,^,gh-proxy-manager-$(VERSION)/,' \
		-czf gh-proxy-manager-$(VERSION).tar.gz .

packages: build
	mkdir -p dist
	VERSION=$(VERSION) $(NFPM) package --config packaging/nfpm.yaml --packager deb --target dist/
	VERSION=$(VERSION) $(NFPM) package --config packaging/nfpm.yaml --packager rpm --target dist/
	VERSION=$(VERSION) $(NFPM) package --config packaging/nfpm.yaml --packager archlinux --target dist/

appimage: build
	bash packaging/appimage/build-appimage.sh $(VERSION) dist

clean:
	cargo clean
	rm -rf dist
	rm -f gh-proxy-manager-*.tar.gz *.pkg.tar.zst