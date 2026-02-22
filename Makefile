.PHONY: lint lint-fix docs-generate docs-serve

lint:
	cargo fmt -- --check
	cargo clippy --all-targets --all-features -- -D warnings

lint-fix:
	cargo fmt
	cargo clippy --fix --allow-dirty --allow-staged

docs-generate:
	./scripts/generate-docs.sh

docs-serve:
	mkdocs serve
