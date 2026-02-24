# Docs Workflow

## Run docs locally

```sh
python -m pip install --upgrade pip
python -m pip install mkdocs-material mdformat mdformat-gfm
./scripts/generate-docs.sh
mkdocs serve
```

Site URL (default): <http://127.0.0.1:8000>

## Regenerate reference docs

```sh
./scripts/generate-docs.sh
```

Generated files:

- `docs/reference/cli.md`
- `docs/reference/config.md`

Generation is deterministic; rerunning should not change output unless CLI/config source changed.

## CI docs checks

CI performs:

1. Generation refresh (`./scripts/generate-docs.sh`)
2. Markdown format check (`mdformat --check`)
3. Markdown lint check (`markdownlint-cli2`)
4. Internal link validation (`mkdocs build --strict`)
5. Generated-file freshness (`git diff --exit-code` on generated docs)

## Publishing

Docs publish from `main` via GitHub Pages workflow (`.github/workflows/docs-publish.yml`).

Required repository settings:

- Pages source: `GitHub Actions`

No additional secrets are required for default GitHub Pages deployment.

For pull requests, the workflow uploads a preview artifact (`site/`) to the workflow run.

## Troubleshooting

- `mkdocs: command not found`: install Python dependencies above
- `docs/reference/*.md` changed unexpectedly: rerun generation and commit updated files
- Markdown lint failures: run formatter, then rerun lint
