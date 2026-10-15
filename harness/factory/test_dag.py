import unittest
from dag import validate_graph, validate_leases, find_cycle


def t(i, deps=(), **kw):
    base = {"id": i, "title": i, "epic": "e", "integration_dependencies": list(deps),
            "contract_dependencies": [], "ownership_domain": "d", "owned_path_glob": "x/**",
            "deliverable": "d", "deferred_verification_family": "f", "state": "PLANNED"}
    base.update(kw)
    return base


class DagTests(unittest.TestCase):
    def test_valid(self):
        errs, stats = validate_graph({"tasks": [t("A"), t("B", ["A"])]})
        self.assertEqual(errs, [])
        self.assertEqual(stats["integration_edges"], 1)

    def test_cycle_detected(self):
        errs, _ = validate_graph({"tasks": [t("A", ["B"]), t("B", ["A"])]})
        self.assertTrue(any(e["kind"] == "cycle" for e in errs))

    def test_unknown_dep_and_dup(self):
        errs, _ = validate_graph({"tasks": [t("A", ["Z"]), t("A")]})
        kinds = {e["kind"] for e in errs}
        self.assertIn("unknown_dependency", kinds)
        self.assertIn("duplicate_id", kinds)

    def test_lease_overlap(self):
        docs = [{"leases": [{"lease_id": "L1", "task_ids": ["A"], "files": ["crates/x/src/a.rs"]},
                            {"lease_id": "L2", "task_ids": ["A"], "files": ["crates/x/src/a.rs", "crates/x/src/lib.rs"]}]}]
        errs, _ = validate_leases(docs, {"A"})
        kinds = {e["kind"] for e in errs}
        self.assertIn("lease_overlap", kinds)
        self.assertIn("lease_on_protected_path", kinds)

    def test_find_cycle_none(self):
        self.assertIsNone(find_cycle(["A", "B"], {"B": ["A"]}))


if __name__ == "__main__":
    unittest.main()
