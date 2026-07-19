# rspacefs-csi — build the driver binary and container image.
# The build host uses podman (no docker); override CONTAINER_ENGINE if needed.

CONTAINER_ENGINE ?= podman
REGISTRY         ?= qregistry.local
IMAGE_NAME       ?= rspacefs-csi
VERSION          ?= $(shell awk -F\" '/^version/ {print $$2; exit}' Cargo.toml)
IMAGE            ?= $(REGISTRY)/$(IMAGE_NAME):$(VERSION)

CARGO   ?= cargo
TARGET  ?=
CARGO_TARGET_FLAG := $(if $(TARGET),--target $(TARGET),)
TARGET_SUBDIR     := $(if $(TARGET),$(TARGET)/,)
BIN := target/$(TARGET_SUBDIR)release/rspacefs-csi

.PHONY: all build release test fmt fmt-check clippy image push clean dist

all: build

build:
	$(CARGO) build --workspace $(CARGO_TARGET_FLAG)

release:
	$(CARGO) build --workspace --release $(CARGO_TARGET_FLAG)

test:
	$(CARGO) test --workspace

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

image:
	$(CONTAINER_ENGINE) build -t $(IMAGE) .

push:
	$(CONTAINER_ENGINE) push $(IMAGE)

# Stage release artifacts (stripped binary + checksum) under dist/.
dist: release
	@mkdir -p dist
	@cp $(BIN) dist/rspacefs-csi-$(VERSION)-$(if $(TARGET),$(TARGET),$(shell rustc -vV | awk '/host:/ {print $$2}'))
	@cd dist && for f in rspacefs-csi-*; do case "$$f" in *.sha256) continue;; esac; sha256sum "$$f" > "$$f.sha256"; done
	@ls -l dist

clean:
	$(CARGO) clean
	rm -rf dist
