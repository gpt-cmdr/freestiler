# Python Release Checklist

This package publishes Python distributions to PyPI with the default native
feature set.

Current wheel policy:

- Published wheels include the default feature set.
- GeoParquet support is included.
- DuckDB support is included.
- Supported wheel targets are Python 3.9 through 3.14.

## Before releasing

1. Bump the Python version in `python/pyproject.toml` and `python/Cargo.toml`.
2. Review `python/README.md` for any release-specific wording.
3. Ensure the git tree is clean enough for a release build.
4. Confirm the GitHub Actions trusted publisher path is still what you want:
   repo `walkerke/freestiler`, workflow `.github/workflows/python-package.yml`,
   environment `pypi`.

## Build and validate locally

```bash
cd python
python3 -m venv .venv
source .venv/bin/activate
pip install -e '.[dev]'
python -m pytest -q tests
python -m maturin build --release --sdist --out dist
python -m twine check dist/*
```

## TestPyPI upload

```bash
python -m twine upload --repository testpypi dist/*
```

## PyPI upload

Preferred path: push a `python-v*` tag and let GitHub Actions publish via the
trusted publisher workflow in `.github/workflows/python-package.yml`.

Typical release flow:

```bash
git add .
git commit -m "python release v0.1.0"
git push origin main
git tag python-v0.1.0
git push origin python-v0.1.0
```

If trusted publishing is configured on PyPI for this repository and workflow,
you do not need to sign in locally or remember an API token. GitHub Actions
publishes directly.

If you are unsure whether trusted publishing is configured, check the PyPI
project settings once in the browser before tagging. The project should list a
trusted publisher for this repository/workflow combination.

Manual fallback:

```bash
python -m twine upload dist/*
```

For the manual fallback, use a PyPI API token rather than a password.

## gpt-cmdr fork releases

The `gpt-cmdr/freestiler` fork never publishes to PyPI or crates.io: the
`freestiler` name belongs to upstream. Fork builds carry the upstream base
version plus a fourth component (`0.3.0.01`, which PEP 440 normalizes to
`0.3.0.1`; `python/Cargo.toml` records it as `0.3.0+gpt-cmdr.1`) and are
published only as GitHub Release assets on the fork.

1. Set the version in `python/pyproject.toml` and `python/Cargo.toml`
   (and the `freestiler-python` entry in `python/Cargo.lock`).
2. Tag `vX.Y.Z.N-gpt-cmdr`. Never push a `python-v*` tag on the fork: that
   prefix triggers the PyPI publish job.
3. Run the **Python Package** workflow manually on the tag
   (`gh workflow run python-package.yml -R gpt-cmdr/freestiler --ref <tag>`);
   the publish job is skipped because the ref is not `python-v*`.
4. Download the wheel and sdist artifacts, write `SHA256SUMS`, and attach them
   to a GitHub Release for the tag.
