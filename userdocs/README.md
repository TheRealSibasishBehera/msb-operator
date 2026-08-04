# User documentation

The user-facing docs for the msb-operator, built with [Mintlify](https://mintlify.com).
These are distinct from the design docs under `docs/`, which are contributor
source-of-truth, not user-facing.

## Preview locally

```bash
npm i -g mint      # once
cd userdocs
mint dev           # serves http://localhost:3000
```

The site hot-reloads as you edit `.mdx` files.

## Structure

- `docs.json` — navigation and theme (the information architecture).
- `getting-started/`, `sandboxes/`, `access/`, `security/` — the Documentation tab.
- `reference/` — the Reference tab. `sandbox-spec` and `sandbox-status` are
  **generated from the CRD** and must not be hand-edited (see the design doc).
- `install/`, `changelog/` — operator install and release notes.

## Do not hand-edit the generated reference

`reference/sandbox-spec.mdx` and `reference/sandbox-status.mdx` are produced from
the `Sandbox` CRD, which carries every field's description from the Rust
doc-comments on `crates/msb-crd/src/sandbox.rs`. To change a field's reference
entry, edit its doc-comment and regenerate — never edit the page.

The plan and rationale live in `docs/user-docs-design.md`.
