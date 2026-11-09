from __future__ import annotations

import os
import sys
from dataclasses import dataclass, field
from datetime import date
from enum import IntEnum


class Priority(IntEnum):
    LOW = 1
    NORMAL = 2
    HIGH = 3


@dataclass
class Task:
    title: str
    priority: Priority = Priority.NORMAL
    due: date | None = None
    done: bool = False
    tags: set[str] = field(default_factory=set)

    def is_overdue(self, today: date) -> bool:
        return not self.done and self.due is not None and self.due < today


class TaskList:
    def __init__(self) -> None:
        self._tasks: list[Task] = []

    def add(self, title: str, priority: Priority = Priority.NORMAL, due: date | None = None, *tags: str) -> Task:
        if not title.strip():
            raise ValueError("title must not be empty")
        task = Task(title.strip(), priority, due, tags=set(tags))
        self._tasks.append(task)
        return task

    def complete(self, title: str) -> Task:
        for task in self._tasks:
            if task.title == title:
                task.done = True
                return task
        raise KeyError(title)

    def pending(self) -> list[Task]:
        # TODO: stable ordering for equal priorities (by due date)
        return sorted((t for t in self._tasks if not t.done), key=lambda t: -t.priority)

    def overdue(self, today: date) -> list[Task]:
        return [t for t in self._tasks if t.is_overdue(today)]

    def by_tag(self, tag: str) -> list[Task]:
        return [t for t in self._tasks if tag in t.tags]

    def __len__(self) -> int:
        return len(self._tasks)
