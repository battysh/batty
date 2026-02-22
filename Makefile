.PHONY: lint lint-fix

lint:
	cargo fmt -- --check
	cargo clippy --all-targets --all-features -- -D warnings

lint-fix:
	cargo fmt
	cargo clippy --fix --allow-dirty --allow-staged
