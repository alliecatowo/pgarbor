# pgarbor-plan amendment: MERGE planning and WITH CHECK OPTIONs

- `PlanKind::Merge`, `ModifyOp::Merge`, `ModifyTableNode.merge: Option<MergeSpec>` (`MergePlanAction`, `MergePlanActionKind`).
- `ModifyTarget.wco: Vec<TargetWco>` rewritten over the new row like `checks`.
- `join::finish_join` restores the logical column order from the physical layout (keeps rowid slots).
