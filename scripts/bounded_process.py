"""Finite local subprocesses, including process-group cleanup on cancellation."""
import os
import signal
import subprocess


def run(args, *, timeout=30, input=None, stdin=None, stdout=subprocess.PIPE):
    child = subprocess.Popen(args, stdin=subprocess.PIPE if input is not None else stdin,
                             stdout=stdout, stderr=subprocess.PIPE, start_new_session=True)
    try:
        out, err = child.communicate(input, timeout=timeout)
    except BaseException:
        # A descendant may still own stdout even after the direct child exits.
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(child.pid, sig)
            except ProcessLookupError:
                pass
            if sig == signal.SIGTERM:
                try:
                    child.communicate(timeout=1)
                except subprocess.TimeoutExpired:
                    pass
        child.communicate(timeout=5)
        raise
    if child.returncode:
        # Arguments, stdin and stderr can contain credentials; never echo them.
        raise ValueError(f'{os.path.basename(str(args[0]))} failed (exit {child.returncode})')
    return out
