.PHONY: example-config dev build build-dependencies linux-system-dependencies

example-config:
# This doesn't work
	export guard_config=$(cat ../example/guard-dev-config.toml)

dev:
# TODO: Conditioninally add "RUST_LOG" variable, only add it if RUST_LOG is empty (e.g you'd add if user ran "make dev" instead of "RUST_LOG=info make dev")
	cd server && RUST_LOG=info cargo run -- --port 8091

build-dependencies:
	curl --proto '=https' --tlsv1.2 https://sh.rustup.rs -sSf | sh -s -- -y
	. "$(HOME)/.cargo/env"

linux-system-dependencies:
	apt update -y && apt install default-libmysqlclient-dev pkg-config -y
	
build:
	rustc --version && cargo --version  # For any future debugging.
	apt update -y && apt install zip tree -y
	tree .
	cd $(BASE)/server && \
		cargo build --verbose --release && \
		cargo test --verbose
	mkdir $(BASE)/release
	apt-get update -y && \
		apt-get install -y build-essential curl file git unzip && \
	mv $(BASE)/server/target/release/seer-server $(BASE)/release
	mv $(BASE)/server/filters.json $(BASE)/release

	cd $(BASE)/release && zip -r ../seer.zip .