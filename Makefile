.PHONY: lint lint-fix test coverage release docs-generate docs-serve

lint:
	cargo fmt -- --check
	cargo clippy --all-targets --all-features -- -D warnings

lint-fix:
	cargo fmt
	cargo clippy --fix --allow-dirty --allow-staged

test:
	cargo test

coverage:
	cargo tarpaulin --skip-clean --out Html --include-tests \
		-- \
		--skip tmux::tests::create_session \
		--skip tmux::tests::create_window \
		--skip tmux::tests::capture_pane \
		--skip tmux::tests::send_keys \
		--skip tmux::tests::session_with_short \
		--skip orchestrator::tests::status_bar \
		--skip orchestrator::tests::handle_prompt_tier2 \
		--skip orchestrator::tests::harness_direct_reply \
		--skip tier2::tests::call_supervisor \
		--skip work::phase_worktree::tests::prepare_agent_worktrees_creates \
		--skip team::daemon::tests::startup_cwd_validation_corrects_all_agent_panes \
		--skip team::daemon::tests::restart_member_corrects_mismatched_cwd_after_respawn \
		--skip worktree::tests::branch_fully_merged \
		--skip worktree::tests::reset_worktree_to_base \
		--skip team::daemon::health::tests::check_backend_health_emits_event_on_transition \
		--skip team::daemon::health::tests::check_backend_health_no_event_when_state_unchanged \
		--skip team::daemon::health::tests::uncommitted_diff_lines

release:
ifndef VERSION
	$(error VERSION is required. Usage: make release VERSION=x.y.z)
endif
	sed -i '' 's/^version = ".*"/version = "$(VERSION)"/' Cargo.toml
	sed -i '' 's/^## Unreleased/## $(VERSION) — $(shell date +%Y-%m-%d)/' CHANGELOG.md
	cargo build --release
	git commit -am 'release: v$(VERSION)'
	git tag v$(VERSION)

docs-generate:
	./scripts/generate-docs.sh

docs-serve:
	mkdocs serve
