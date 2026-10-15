"""FAC-01 tests: integration waves, speculative/integration readiness, fanout, repair merge."""

import copy
import unittest
from pathlib import Path

from pgarbor_factory.dag import (
    DagError, fanout, integration_ready, integration_waves, load_graph, merge_repair_nodes,
    speculative_ready,
)

REPO = Path(__file__).resolve().parents[2]
REAL_GRAPH = REPO / "contracts" / "dag" / "task_graph.v1.json"


def t(i, deps=(), contract=(), state="PLANNED", **kw):
    base = {"id": i, "title": i, "epic": "e", "integration_dependencies": list(deps),
            "contract_dependencies": list(contract), "ownership_domain": "d",
            "owned_path_glob": "x/**", "deliverable": "d",
            "deferred_verification_family": "f", "state": state}
    base.update(kw)
    return base


def ticket(i, deps=()):
    return {"id": i, "title": i, "epic": "repair", "integration_dependencies": list(deps),
            "contract_dependencies": [], "ownership_domain": "d", "owned_path_glob": "x/**",
            "deliverable": "d", "deferred_verification_family": "f", "state": "PLANNED",
            "root_cause": "r", "severity": "P1", "expected_unblocked": 1, "dependency_fanout": 1,
            "confidence": 1.0, "cost": 1.0, "score": 1.0, "critical": False, "repro": [],
            "expected_behavior": "e", "actual_behavior": "a", "owned_files": [], "leases": []}


class WavesTests(unittest.TestCase):
    def test_small_graph_layers(self):
        g = {"tasks": [t("A"), t("B", ["A"]), t("C", ["A"]), t("D", ["B", "C"]), t("E")]}
        self.assertEqual(integration_waves(g), [["A", "E"], ["B", "C"], ["D"]])

    def test_empty_graph(self):
        self.assertEqual(integration_waves({"tasks": []}), [])

    def test_cycle_raises(self):
        g = {"tasks": [t("A", ["B"]), t("B", ["A"]), t("C")]}
        with self.assertRaises(DagError):
            integration_waves(g)

    def test_self_dependency_raises(self):
        with self.assertRaises(DagError):
            integration_waves({"tasks": [t("A", ["A"])]})

    def test_unknown_dependency_raises(self):
        with self.assertRaises(DagError):
            integration_waves({"tasks": [t("A", ["Z"])]})

    def test_real_graph_covers_all_tasks(self):
        g = load_graph(REAL_GRAPH)
        waves = integration_waves(g)
        flat = [i for w in waves for i in w]
        ids = [x["id"] for x in g["tasks"]]
        self.assertEqual(len(flat), 209)
        self.assertEqual(len(set(flat)), 209)
        self.assertEqual(sorted(flat), sorted(ids))
        position = {i: n for n, w in enumerate(waves) for i in w}
        for x in g["tasks"]:
            for d in x["integration_dependencies"]:
                self.assertLess(position[d], position[x["id"]])


class ReadinessTests(unittest.TestCase):
    def test_speculative_ignores_integration_deps(self):
        g = {"tasks": [t("A", ["B"]), t("B"), t("C", contract=["K"]),
                       t("D", contract=["K"]), t("E", state="VERIFIED")]}
        self.assertEqual(speculative_ready(g, set()), ["A", "B"])
        self.assertEqual(speculative_ready(g, {"K"}), ["A", "B", "C", "D"])

    def test_speculative_states(self):
        g = {"tasks": [t("A", state="SPEC_READY"), t("B", state="SPECULATING"),
                       t("C", state="BLOCKED")]}
        self.assertEqual(speculative_ready(g, set()), ["A"])

    def test_integration_ready(self):
        g = {"tasks": [t("A"), t("B", ["A"]), t("C", ["A", "B"])]}
        self.assertEqual(integration_ready(g, set()), ["A"])
        self.assertEqual(integration_ready(g, {"A"}), ["A", "B"])
        self.assertEqual(integration_ready(g, {"A", "B"}), ["A", "B", "C"])


class FanoutTests(unittest.TestCase):
    def test_transitive_counts(self):
        g = {"tasks": [t("A"), t("B", ["A"]), t("C", ["B"]), t("D", ["A"]), t("E")]}
        self.assertEqual(fanout(g), {"A": 3, "B": 1, "C": 0, "D": 0, "E": 0})

    def test_diamond_counts_each_dependent_once(self):
        g = {"tasks": [t("A"), t("B", ["A"]), t("C", ["A"]), t("D", ["B", "C"])]}
        self.assertEqual(fanout(g), {"A": 3, "B": 1, "C": 1, "D": 0})

    def test_real_graph_keys(self):
        g = load_graph(REAL_GRAPH)
        counts = fanout(g)
        self.assertEqual(sorted(counts), sorted(x["id"] for x in g["tasks"]))


class MergeRepairTests(unittest.TestCase):
    def test_appends_new_ticket_without_mutating_input(self):
        g = {"tasks": [t("A")]}
        before = copy.deepcopy(g)
        new, errors = merge_repair_nodes(g, [ticket("REPAIR-1", ["A"])])
        self.assertEqual(errors, [])
        self.assertEqual([x["id"] for x in new["tasks"]], ["A", "REPAIR-1"])
        self.assertEqual(g, before)

    def test_rejects_existing_id(self):
        g = {"tasks": [t("A")]}
        new, errors = merge_repair_nodes(g, [ticket("A")])
        self.assertIn({"kind": "duplicate_ticket_id", "id": "A"}, errors)
        self.assertEqual(len(new["tasks"]), 1)

    def test_rejects_repeated_id_within_batch(self):
        g = {"tasks": [t("A")]}
        new, errors = merge_repair_nodes(g, [ticket("R1"), ticket("R1")])
        self.assertEqual(sum(1 for e in errors if e["kind"] == "duplicate_ticket_id"), 1)
        self.assertEqual([x["id"] for x in new["tasks"]], ["A", "R1"])

    def test_reports_unknown_dependency_of_ticket(self):
        g = {"tasks": [t("A")]}
        _, errors = merge_repair_nodes(g, [ticket("R1", ["NOPE"])])
        self.assertTrue(any(e["kind"] == "unknown_dependency" for e in errors))


class LoadGraphTests(unittest.TestCase):
    def test_real_graph_loads(self):
        g = load_graph(REAL_GRAPH)
        self.assertEqual(len(g["tasks"]), 209)

    def test_missing_file_raises(self):
        with self.assertRaises(DagError):
            load_graph(REPO / "contracts" / "dag" / "does-not-exist.json")


if __name__ == "__main__":
    unittest.main()
