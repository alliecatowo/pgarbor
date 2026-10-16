# pgarbor-plan amendment: inheritance and partition scans

- `PlannerState.inherit_scans: Vec<(level, rt_index)>` marks scans of a table with children; `inherit.rs` expands
  them into an Append over the parent and its descendants (partitions are bound-pruned), remapping columns by name
  (dropped parent columns read as NULL) and exposing `tableoid` per child.
- `tableoid` may also be selected from virtual catalog relations (constant OID).
