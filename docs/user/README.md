# User documentation

The user-facing docs for msb-operator, built with [Mintlify](https://mintlify.com).

## Run the site locally

From the repository root:

```bash
npm i -g mint      # once
cd docs/user
mint dev           # serves http://localhost:3000
```

The site hot-reloads as you edit `.mdx` files.

## Structure

- `docs.json` — navigation and theme.
- `getting-started/`, `workloads/`, `netstorage/`, `install/`, `changelog/` — the guides.
- `reference/` — `configuration`, `kubectl-recipes`, and the `sandbox-spec` /
  `sandbox-status` pages generated from the CRD (see below).

## Do not hand-edit the generated reference

`reference/sandbox-spec.mdx` and `reference/sandbox-status.mdx` are produced from
the `Sandbox` CRD, which carries every field's description from the Rust
doc-comments on `crates/msb-crd/src/sandbox.rs`. To change a field's reference
entry, edit its doc-comment and regenerate the page rather than editing it here.
