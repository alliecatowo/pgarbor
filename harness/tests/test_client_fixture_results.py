import json
import pytest
from pgarbor_factory.campaign import _parse_client_lines


def summary(**kw):
    return "FIXTURE_RESULT " + json.dumps(dict(name="app", passed=2, failed=0, steps=2, **kw))


def test_real_prefixed_summary_accepted():
    assert _parse_client_lines("install log\n" + summary())[0][:2] == ("app", "pass")


@pytest.mark.parametrize("text", ["", "{}", "FIXTURE_RESULT broken", summary()+"\n"+summary(),
    'FIXTURE_RESULT {"name":"app","passed":0,"failed":0,"steps":0}',
    'FIXTURE_RESULT {"name":"app","passed":2,"failed":0,"steps":3}',
    'FIXTURE_RESULT {"name":"app","passed":true,"failed":0,"steps":1}',
    'FIXTURE_RESULT {"name":"app","passed":-1,"failed":2,"steps":1}'])
def test_missing_malformed_duplicate_or_vacuous_summary_rejected(text):
    with pytest.raises(ValueError): _parse_client_lines(text)


def test_failed_checks_reject_successful_process_summary():
    line = 'FIXTURE_RESULT {"name":"app","passed":1,"failed":1,"steps":2}'
    assert _parse_client_lines(line)[0][1] == "fail"


@pytest.mark.parametrize("returncode,failed,expected", [(0, 0, "PASS"), (1, 0, "MISMATCH"),
                                                         (0, 1, "MISMATCH")])
def test_campaign_discovers_actual_apps_sets_url_and_keeps_output(tmp_path, monkeypatch,
                                                                  returncode, failed, expected):
    from types import SimpleNamespace
    from pgarbor_factory import campaign
    from pgarbor_harness.config import Paths
    app = tmp_path / "tests" / "apps" / "app"
    app.mkdir(parents=True)
    (app / "run.sh").write_text("exit 0")
    provisioned = []
    released = []
    endpoint = SimpleNamespace(user="postgres", password=None, host="localhost", port=5432,
                               dbname="fresh")
    env = SimpleNamespace(endpoint=endpoint)
    target = SimpleNamespace(start=lambda: None, stop=lambda: None,
                             provision=lambda name: provisioned.append(name) or env,
                             release=lambda e: released.append(e))
    monkeypatch.setattr(campaign, "_make_target", lambda *a: target)
    def subprocess_run(argv, **kw):
        assert argv == ["bash", str(app / "run.sh")]
        assert kw["env"]["DATABASE_URL"] == "postgresql://postgres@localhost:5432/fresh"
        assert kw["cwd"] == str(app)
        output = "FIXTURE_RESULT " + json.dumps(dict(name="app", passed=2-failed,
                                                       failed=failed, steps=2))
        return SimpleNamespace(stdout=output, stderr="diagnostic", returncode=returncode)
    monkeypatch.setattr(campaign.subprocess, "run", subprocess_run)
    ctx = campaign._Ctx(tmp_path, Paths(root=tmp_path), tmp_path / "run", "probe", "sha")
    family, = campaign._phase_clients(campaign.CampaignConfig(tmp_path), ctx)
    assert family.counts[expected] == 1
    assert family.total_cataloged == 1
    assert provisioned == ["client-app"] and released == [env]
    assert (ctx.run_dir / "clients" / "app" / "stderr.txt").read_text() == "diagnostic"
