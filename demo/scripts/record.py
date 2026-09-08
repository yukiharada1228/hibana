#!/usr/bin/env python3
"""Execute and record a real, disposable Hibana lifecycle for the Remotion film."""
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shlex
import shutil
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
DEMO = ROOT / "demo"
STATE = ROOT / ".local/demo-recording-current"
APP = STATE / "hello-hono"
CLUSTER = "hibana-demo"
PLATFORM = ROOT / ".local/kubernetes-hibana-demo"
RECORD = DEMO / "public/recording.json"
env = os.environ.copy()
env["HIBANA_CONFIG_HOME"] = str(APP / ".hibana/cli")
env.pop("HIBANA_PROFILE", None)
hidden = []
data = {"format": 1, "recordedAt": datetime.now(timezone.utc).isoformat(),
        "cluster": CLUSTER, "source": (DEMO / "app.ts").read_text(), "steps": [], "proof": {}, "complete": False}


def sanitize(value):
    value = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", value).replace(str(ROOT), "<checkout>")
    for secret in hidden:
        value = value.replace(secret, "[REDACTED]")
    return value


def save():
    RECORD.parent.mkdir(parents=True, exist_ok=True)
    RECORD.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")


def command(id, args, cwd=ROOT, timeout=900):
    if args[0] == "kubectl":
        args = [args[0], "--request-timeout=15s", *args[1:]]
    display = shlex.join([str(x) for x in args])
    print(f"Recording {id}: {sanitize(display)}", flush=True)
    start = time.monotonic()
    child = subprocess.Popen([str(a) for a in args], cwd=cwd, env=env, text=False,
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
    events, chunks = [], []
    selector = selectors.DefaultSelector()
    selector.register(child.stdout, selectors.EVENT_READ)
    try:
        while selector.get_map():
            if time.monotonic() - start > timeout:
                raise TimeoutError(f"{id} exceeded {timeout}s")
            for key, _ in selector.select(0.2):
                chunk = os.read(key.fd, 65536)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                text = chunk.decode("utf8", errors="replace")
                chunks.append(text)
                events.append({"t": round(time.monotonic()-start, 3), "text": sanitize(text)})
        code = child.wait(timeout=10)
    except BaseException:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        except PermissionError:
            child.terminate()
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except PermissionError:
                child.kill()
            child.wait()
        raise
    finally:
        selector.close()
        child.stdout.close()
    output = "".join(chunks)
    data["steps"].append({"id": id, "command": sanitize(display), "cwd": sanitize(str(cwd)),
                          "durationSeconds": round(time.monotonic()-start, 3),
                          "output": sanitize(output), "exitCode": code, "events": events})
    save()
    print(sanitize("\n".join(output.strip().splitlines()[-5:])), flush=True)
    if code:
        raise RuntimeError(f"{id} exited with {code}; see {RECORD.name}")
    return output


def workers(id):
    output = command(id, ["kubectl", "-n", "hibana", "get", "pods", "-l",
                          "app.kubernetes.io/name=hibana-worker", "-o", "json"])
    return [{"name": p["metadata"]["name"], "uid": p["metadata"]["uid"],
             "node": p["spec"]["nodeName"], "command": p["spec"]["containers"][0]["command"],
             "imageId": p["status"]["containerStatuses"][0]["imageID"],
             "ready": all(c["ready"] for c in p["status"]["containerStatuses"])}
            for p in json.loads(output)["items"]]


def main():
    clusters = subprocess.check_output([str(ROOT / ".local/bin/kind"), "get", "clusters"], text=True).splitlines()
    if CLUSTER in clusters or APP.exists():
        raise RuntimeError("Demo target already exists. Inspect it before starting a fresh recording.")
    STATE.mkdir(parents=True, exist_ok=True, mode=0o700)
    created = False
    try:
        created = True
        command("install", ["hibana", "platform", "install", "--source", str(ROOT), "--cluster", CLUSTER])
        for entry in shlex.split((PLATFORM / "sdk.env").read_text()):
            key, value = entry.split("=", 1)
            env[key] = value
            if any(word in key for word in ("TOKEN", "PASSWORD", "SECRET")):
                hidden.append(value)
        env.pop("HIBANA_TOKEN", None)
        env["KUBECONFIG"] = str(PLATFORM / "kubeconfig")
        command("status", ["hibana", "platform", "status", "--source", str(ROOT), "--cluster", CLUSTER])
        command("init", ["hibana", "init", "hello-hono", "--template", "hono", "--cli-package", str(ROOT / "sdk")], cwd=STATE)
        (APP / "src/index.ts").write_text(data["source"])
        command("source", ["cat", "src/index.ts"], cwd=APP)
        command("login", ["hibana", "login"], cwd=APP)
        before = workers("workers-before")
        workloads_before = json.loads(command("workloads-before", ["kubectl", "-n", "hibana", "get", "deployments", "-o", "json"]))
        command("deploy", ["hibana", "deploy", "--version", "1.0.0"], cwd=APP)
        artifact = APP / ".hibana/build/app.wasm"
        binary = artifact.read_bytes()
        assert binary[:8] == bytes([0,97,115,109,13,0,1,0]), "Not a Wasm Component"
        command("wasm-header", ["xxd", "-l", "8", ".hibana/build/app.wasm"], cwd=APP)
        wit = command("wit", [ROOT / "sdk/node_modules/.bin/jco", "wit", ".hibana/build/app.wasm"], cwd=APP)
        export = next(line.strip() for line in wit.splitlines() if "export wasi:http/incoming-handler" in line)
        data["proof"]["artifact"] = {"bytes": len(binary), "sha256": hashlib.sha256(binary).hexdigest(),
                                       "header": binary[:8].hex(" "), "export": export}
        # Preparation must finish without using a guest request as warm-up.
        prepared = []
        for worker in before:
            cache = command(f"prepared-{worker['name']}", ["kubectl", "-n", "hibana", "exec", worker["name"], "--", "ls", "/var/cache/hibana"])
            assert data["proof"]["artifact"]["sha256"] + ".cwasm" in cache
            prepared.append(worker["name"])
        count = command("no-warmup-executions", ["kubectl", "-n", "hibana", "exec", "deployment/hibana-postgres", "--",
            "psql", "-U", "hibana_admin", "-d", "hibana", "-Atc", "SELECT count(*) FROM executions"])
        assert count.strip() == "0"
        data["proof"].update({"preparedBeforeRequests": True, "preparedWorkers": prepared, "warmupExecutions": 0})
        responses, timings = [], []
        for index in range(3):
            output = command(f"request-{index+1}", ["curl", "--silent", "--show-error", "--max-time", "60", "-i",
                "-w", "\n__TIME_TOTAL__%{time_total}\n",
                "-H", "Host: hello-hono.smoke.hibana.local", "http://127.0.0.1:18084/"], cwd=APP)
            http, timing = output.rsplit("\n__TIME_TOTAL__", 1)
            response = json.loads(re.split(r"\r?\n\r?\n", http, maxsplit=1)[1])
            assert "200 OK" in output and response == {"message": "Hello, Wasm!", "count": 1}
            responses.append(response)
            timings.append(round(float(timing.strip()) * 1000, 3))
        after = workers("workers-after")
        assert len(before) == len(after) == 2
        assert {p["uid"] for p in before} == {p["uid"] for p in after}
        workloads_after = json.loads(command("workloads-after", ["kubectl", "-n", "hibana", "get", "deployments", "-o", "json"]))
        assert {p['metadata']['uid'] for p in workloads_before['items']} == {p['metadata']['uid'] for p in workloads_after['items']}
        query = "SELECT COALESCE(json_agg(row_to_json(r)),'[]'::json) FROM (SELECT e.id,e.status,e.http_request,e.wall_time_ms,e.peak_memory_bytes,v.wasm_sha256 FROM executions e JOIN components c ON c.id=e.component_id JOIN component_versions v ON v.id=e.version_id WHERE c.name='hello-hono' ORDER BY e.created_at) r"
        executions = json.loads(command("executions", ["kubectl", "-n", "hibana", "exec", "deployment/hibana-postgres", "--",
                    "psql", "-U", "hibana_admin", "-d", "hibana", "-Atc", query]))
        assert len(executions) == 3
        assert all(e["status"] == "succeeded" and e["http_request"] and e["wasm_sha256"] == data["proof"]["artifact"]["sha256"] for e in executions)
        logs = {p['name']: command(f"worker-log-{p['name']}", ["kubectl", "-n", "hibana", "logs", p['name'], "--timestamps=true"]) for p in after}
        for execution in executions:
            owners = [pod for pod, log in logs.items() if execution['id'] in log]
            assert len(owners) == 1
            execution['workerPod'] = owners[0]
        assert all(log.count('component cache: miss; downloading and precompiling') == 1 for log in logs.values())
        data["proof"].update({"workersBefore": before, "workersAfter": after, "responses": responses,
                              "httpTimeMs": timings, "executions": executions, "sameWorkerPods": True,
                              "sameDeployments": True, "compilationsPerWorker": 1,
                              "sourceSha256": {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest()
                                  for p in [ROOT / 'crates/worker/src/runtime/mod.rs', ROOT / 'crates/worker/src/artifacts.rs',
                                            ROOT / 'crates/control-plane/src/preparation.rs']}})
        command("delete", ["hibana", "delete", "hello-hono", "--yes"], cwd=APP)
        remaining = json.loads(command("list-empty", ["hibana", "list"], cwd=APP))
        assert remaining == []
        status = command("http-deleted", ["curl", "--silent", "--show-error", "--max-time", "15", "-o", "/dev/null", "-w", "%{http_code}\n",
            "-H", "Host: hello-hono.smoke.hibana.local", "http://127.0.0.1:18084/"], cwd=APP).strip()
        assert status == "404"
        data["proof"].update({"remainingApplications": remaining, "deletedHttpStatus": 404})
        command("uninstall", ["hibana", "platform", "uninstall", "--source", str(ROOT), "--cluster", CLUSTER, "--yes"])
        created = False
        nodes = command("cluster-empty", ["docker", "ps", "-a", "--filter", f"label=io.x-k8s.kind.cluster={CLUSTER}", "--format", "{{.Names}}"])
        assert not nodes.strip()
        data["proof"]["remainingDemoNodes"] = []
        data["complete"] = True
        save()
        print("PASS: authentic lifecycle recorded; Wasm hash, HTTP isolation, shared Pods, deletion and teardown verified", flush=True)
    finally:
        if created:
            command("cleanup-after-failure", ["hibana", "platform", "uninstall", "--source", str(ROOT), "--cluster", CLUSTER, "--yes"])
        if data["complete"] and APP.is_dir():
            # Keep the measured Wasm for later verification, but remove credentials
            # and the disposable application's dependency tree.
            shutil.copyfile(APP / ".hibana/build/app.wasm", STATE / "app.wasm")
            shutil.rmtree(APP)
        save()


if __name__ == "__main__":
    main()
