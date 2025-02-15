.POSIX:

# Cargo binary
CARGO = cargo

# zkas compiler binary
ZKAS = ../../../zkas

# zkas circuits
PROOFS_SRC = $(shell find proof -type f -name '*.zk')
PROOFS_BIN = $(PROOFS_SRC:=.bin)

# wasm source files
WASM_SRC = \
	$(shell find src -type f) \
	$(shell find ../../sdk -type f) \
	$(shell find ../../serial -type f)

# wasm contract binary
WASM_BIN = money_contract.wasm

all: $(WASM_BIN)

$(WASM_BIN): $(WASM_SRC) $(PROOFS_BIN)
	$(CARGO) build --release --package darkfi-money-contract --target wasm32-unknown-unknown
	cp -f ../../../target/wasm32-unknown-unknown/release/darkfi_money_contract.wasm $@

$(PROOFS_BIN): $(ZKAS) $(PROOFS_SRC)
	$(ZKAS) $(basename $@) -o $@

test-drop-pay-swap: all
	$(CARGO) test --release --features=no-entrypoint,client \
		--package darkfi-money-contract \
		--test drop_pay_swap

bench:
	$(CARGO) test --release --features=no-entrypoint,client \
		--package darkfi-money-contract \
		--test verification_bench $(FILTER)

test: test-drop-pay-swap

clean:
	rm -f $(PROOFS_BIN) $(WASM_BIN)

.PHONY: all test-drop-pay-swap bench test clean
