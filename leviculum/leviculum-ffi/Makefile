# Install layout for libleviculum, the Leviculum C API.
#
# Produces the "feels like a normal Unix C library" install: a versioned
# shared object with SONAME symlinks, the generated header, and a pkg-config
# file, so a consumer can `pkg-config --cflags --libs leviculum` and link with
# `-lleviculum`.
#
#   make build        # build the glibc cdylib (.so) and the header
#   make install      # install under $(PREFIX) (default /usr/local)
#   sudo make install PREFIX=/usr
#
# The cdylib must be built against glibc, not the workspace musl default which
# cannot emit a cdylib, hence the explicit --target.

PREFIX      ?= /usr/local
LIBDIR      ?= $(PREFIX)/lib
INCLUDEDIR  ?= $(PREFIX)/include
PKGCONFIGDIR ?= $(LIBDIR)/pkgconfig
TARGET      ?= x86_64-unknown-linux-gnu

# cargo build honors CARGO_TARGET_DIR; the install rule must read the artifacts
# from the same place, else it copies stale bits from ../target (or fails).
TARGET_DIR  ?= $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),../target)

# Version from the workspace package table; major drives the SONAME.
VERSION       := $(shell sed -n 's/^version = "\(.*\)"/\1/p' ../Cargo.toml | head -1)
SONAME_MAJOR  := 0

SO      := libleviculum.so
STATIC  := libleviculum.a
HEADER  := leviculum.h
BUILT   := $(TARGET_DIR)/$(TARGET)/release/$(SO)
BUILT_A := $(TARGET_DIR)/$(TARGET)/release/$(STATIC)

.PHONY: build install uninstall clean

build:
	cargo build -p leviculum-ffi --release --target $(TARGET)

install: build
	install -d "$(DESTDIR)$(LIBDIR)" "$(DESTDIR)$(INCLUDEDIR)" "$(DESTDIR)$(PKGCONFIGDIR)"
	install -m 0644 "$(BUILT)" "$(DESTDIR)$(LIBDIR)/$(SO).$(VERSION)"
	ln -sf "$(SO).$(VERSION)" "$(DESTDIR)$(LIBDIR)/$(SO).$(SONAME_MAJOR)"
	ln -sf "$(SO).$(SONAME_MAJOR)" "$(DESTDIR)$(LIBDIR)/$(SO)"
	install -m 0644 "$(BUILT_A)" "$(DESTDIR)$(LIBDIR)/$(STATIC)"
	install -m 0644 "$(HEADER)" "$(DESTDIR)$(INCLUDEDIR)/$(HEADER)"
	sed -e 's|@PREFIX@|$(PREFIX)|g' \
	    -e 's|@LIBDIR@|$(LIBDIR)|g' \
	    -e 's|@INCLUDEDIR@|$(INCLUDEDIR)|g' \
	    -e 's|@VERSION@|$(VERSION)|g' \
	    leviculum.pc.in > "$(DESTDIR)$(PKGCONFIGDIR)/leviculum.pc"

uninstall:
	rm -f "$(DESTDIR)$(LIBDIR)/$(SO).$(VERSION)" \
	      "$(DESTDIR)$(LIBDIR)/$(SO).$(SONAME_MAJOR)" \
	      "$(DESTDIR)$(LIBDIR)/$(SO)" \
	      "$(DESTDIR)$(LIBDIR)/$(STATIC)" \
	      "$(DESTDIR)$(INCLUDEDIR)/$(HEADER)" \
	      "$(DESTDIR)$(PKGCONFIGDIR)/leviculum.pc"

clean:
	cargo clean -p leviculum-ffi
