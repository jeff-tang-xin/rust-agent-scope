from __future__ import annotations

import argparse
from datetime import date, timedelta

from .models import Priority, TaskList


def build_demo() -> TaskList:
    today = date.today()
    tasks = TaskList()
    tasks.add("Write release notes", Priority.HIGH, today + timedelta(days=1), "docs")
    tasks.add("Fix flaky test", Priority.HIGH, today - timedelta(days=2), "ci")
    tasks.add("Update dependencies", Priority.LOW, None, "chore")
    return tasks


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="tasks")
    parser.add_argument("--overdue", action="store_true", help="only show overdue tasks")
    args = parser.parse_args(argv)

    tasks = build_demo()
    # Set a breakpoint here (QA: debugger) and inspect `tasks`.
    shown = tasks.overdue(date.today()) if args.overdue else tasks.pending()
    for task in shown:
        flag = "!" if task.priority is Priority.HIGH else " "
        print(f"{flag} {task.title} ({', '.join(sorted(task.tags))})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
