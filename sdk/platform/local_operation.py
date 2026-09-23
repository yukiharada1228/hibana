"""Serialize checkout-owned cluster mutations even while Kubernetes is stopped."""
from datetime import datetime, timezone
import json
import os
import sys
import uuid


class LocalOperationLock:
    """Atomic directory creation; a killed CLI requires explicit recovery.

    A process-scoped OS lock would disappear even if a surviving child command
    still changes the cluster. Keep this lock until the operation unwinds, and
    use a unique record name so an old owner cannot remove a replacement lock.
    """
    def __init__(self, state, action):
        self.path = state / "operation.lock"
        self.record = self.path / f"{uuid.uuid4().hex}.json"
        self.action = action

    def __enter__(self):
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        try:
            self.path.mkdir(mode=0o700)
        except FileExistsError:
            raise ValueError(
                f"Another local platform operation holds the lock at {self.path}. "
                "Wait for it to finish. If its CLI crashed, confirm the CLI and "
                "its child commands have stopped before removing this lock directory and retrying."
            ) from None
        try:
            with self.record.open("x") as stream:
                os.chmod(self.record, 0o600)
                json.dump({"action": self.action, "pid": os.getpid(),
                           "started_at": datetime.now(timezone.utc).isoformat()}, stream)
        except BaseException:
            self.record.unlink(missing_ok=True)
            self.path.rmdir()
            raise
        return self

    def __exit__(self, error_type, error, traceback):
        try:
            self.record.unlink()
            self.path.rmdir()
        except OSError as release_error:
            message = f"Could not release local platform operation lock at {self.path}; inspect it before retrying."
            if error_type is None:
                raise ValueError(message) from release_error
            print(message, file=sys.stderr)
        return False
