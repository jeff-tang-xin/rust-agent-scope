from collections import deque

import pytest

from worker import process, run


def test_process_email():
    assert process({"kind": "email", "to": "a@b.c"}) == "sent email to a@b.c"


def test_unknown_kind():
    with pytest.raises(ValueError):
        process({"kind": "nope"})


def test_run_skips_bad_jobs():
    assert run(deque([{"kind": "report", "name": "x"}, {"kind": "bad"}])) == 1
