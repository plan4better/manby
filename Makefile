# manby's tests.
#
#   make test          the unit tests and the golden tests
#   make test-golden   tippecanoe's golden .json.check tests and the other
#                      recipes of tippecanoe and tile-join, run with manby
#   make test-geobuf   the golden tests again from Geobuf input
#   make test-golden-mlt  the golden tests written as MVT and as MLT, compared
#                      tile by tile (needs Rust 1.98 for the mlt feature)
#
# The golden tests come from tippecanoe-rs (crates/tippecanoe-rs), whose
# tippecanoe-decode decodes the output. Tests that manby does not pass yet
# are listed in golden/skip.txt, with the reason.

CARGO ?= cargo
TIPPECANOE_RS = crates/tippecanoe-rs
RELEASE = target/release
DECODE = $(RELEASE)/tippecanoe-decode
GOLDEN = target/golden

.PHONY: all test unit test-golden test-geobuf test-golden-mlt golden-setup manby tools

all: manby

manby:
	$(CARGO) build --release

# tippecanoe-rs's tools that the recipes use
tools:
	$(CARGO) build --release -p tippecanoe-rs --bin tippecanoe-decode --bin tippecanoe-enumerate --bin tippecanoe-json-tool --example json2geobuf

unit:
	$(CARGO) test --release
	$(CARGO) test --release --features remote --test remote

test: unit test-golden test-geobuf

golden-setup: manby tools
	golden/setup.sh $(TIPPECANOE_RS)/tests $(GOLDEN) target/release/manby $(RELEASE)
	cp golden/skip.txt $(GOLDEN)/skip.txt

test-golden: golden-setup
	$(MAKE) -C $(GOLDEN) -f $(CURDIR)/golden/golden.mk -k all

test-geobuf: golden-setup
	$(MAKE) -C $(GOLDEN) -f $(CURDIR)/golden/golden.mk -k -j1 geobuf-test

# MLT: manby and the comparison built with the mlt feature, in their own target directory
MLT_CARGO = CARGO_TARGET_DIR=target/mlt $(CARGO) +1.98.1

test-golden-mlt: golden-setup
	$(MLT_CARGO) build --release --features mlt --bin manby --example mlt_verify
	ln -sf $(CURDIR)/target/mlt/release/manby $(GOLDEN)/manby-mlt
	ln -sf $(CURDIR)/target/mlt/release/examples/mlt_verify $(GOLDEN)/mlt_verify
	rm -f $(GOLDEN)/mlt-sizes.txt
	$(MAKE) -C $(GOLDEN) -f $(CURDIR)/golden/mlt.mk -k all
