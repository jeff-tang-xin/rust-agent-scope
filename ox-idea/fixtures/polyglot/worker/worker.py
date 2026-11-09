"""Background worker: drains a queue of jobs."""

import argparse
import logging
import time
from collections import deque

log = logging.getLogger("worker")


def process(job: dict) -> str:
    kind = job.get("kind")
    if kind == "email":
        return f"sent email to {job['to']}"
    if kind == "report":
        return f"built report {job['name']}"
    raise ValueError(f"unknown job kind: {kind!r}")


def run(queue: deque, once: bool = False) -> int:
    done = 0
    while queue:
        job = queue.popleft()
        try:
            log.info(process(job))
            done += 1
        except ValueError as e:  # TODO: dead-letter queue
            log.warning("skipping job: %s", e)
        if once:
            break
        time.sleep(0.01)
    return done


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--once", action="store_true")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO)
    jobs = deque([{"kind": "email", "to": "dev@example.com"}, {"kind": "report", "name": "weekly"}, {"kind": "?"}])
    print(f"processed {run(jobs, args.once)} job(s)")
