# 0052: Rows name their signed config root; the dashboard groups by it

Status: accepted (2026-11-16)

## Context

The mesh runs one config, made of config roots the authority signs
(ADR 0049); every instance comes from one of them. A root lives on
`<machine>/<path>`: the authority's original (`~/.devsandboxes`), each
member's copy (`mesh/tree/<name>/`). Snapshot rows carried only `root`,
a path on their own host, so nothing said two hosts' rows were the same
root, and the dashboard grouped by (machine, sandbox): a fleet running
one config showed every sandbox once per host.

## Decision

- **A signed root's id is the name the membership log signs it under**
  (`Op::Config {root}`): the authority's `mesh/roots.toml` binding for
  the path, else the member's copy the path is exactly
  (`fleet::mesh::root_id`). An unsigned root has none.
- **Each host tags its own rows** (`mesh_root` on instance, sandbox and
  service rows, `api::InstancesList::of`): only it knows its bindings and
  copies. Federation passes the field through.
- **The Instances tree groups by config root** (`tui::data::GroupKey`):
  a signed root's sandbox is one group across hosts; an unsigned root's
  is its host's (machine, path). Two roots defining one name are two
  groups.
- **Instances without a recorded root are backfilled at display time**:
  the first signed root's group defining their sandbox, else their
  machine's first row of that name. Nothing on disk is rewritten.

## Consequences

- A merged group's header is one row (this host's when it has the root),
  and header actions route by it; an instance's own Detail, profile and
  identity checks still read its own host's row
  (`remote::instance_sandbox`).
- The tree doesn't render a group's root yet; the key carries it, so two
  same-named groups look alike until it does.
- Trees moved by older placed runs (`fleet/<peer>/roots/<hash>/…`) aren't
  signed roots: their instances group by path, through the fallbacks.
