#!/usr/bin/env python3
"""End-to-end check of server goal control against a synthetic SSE provider.

Starts a real mink-server, creates a session, creates a goal through the HTTP
API, lets the driver run real turns against the synthetic provider, and asserts
that goal_updated events reach the SSE stream in order.
"""
import http.server, json, os, re, shutil, socket, subprocess, sys, tempfile, threading, time, urllib.request, urllib.error

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SERVER = os.path.join(REPO, "target", "debug", "mink-server")


class Provider(http.server.BaseHTTPRequestHandler):
    calls = 0
    goal_ids = []
    reported = set()
    iterations = []
    shapes = []
    # "complete" ends the goal on the third admitted round; "continue" never
    # reports completion, so only the budget can stop the driver.
    mode = "complete"

    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        messages = body.get("messages", [])
        Provider.calls += 1
        turn = Provider.calls
        text = next((str(m.get("content") or "") for m in reversed(messages) if str(m.get("content") or "").startswith("<goal-review>")), "")
        candidate = json.loads(next((line[11:] for line in text.splitlines() if line.startswith("Candidate: ")), "null"))
        contract = json.loads(next((line[10:] for line in text.splitlines() if line.startswith("Contract: ")), "null"))
        names = {t.get("function", t).get("name") for t in body.get("tools", [])}
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()

        def emit(delta, reason=None):
            payload = {"id": "synthetic", "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]}
            self.wfile.write(("data: " + json.dumps(payload) + "\n\n").encode())
            self.wfile.flush()

        try:
            if candidate and "GoalReview" in names and (candidate["goal_id"], candidate["iteration"]) not in Provider.reported:
                round_key = (candidate["goal_id"], candidate["iteration"])
                Provider.reported.add(round_key)
                Provider.iterations.append(candidate["iteration"])
                Provider.goal_ids.append(candidate["goal_id"])
                passed = Provider.mode == "complete" and candidate["iteration"] >= 3
                args = {"original_request":{"status":"pass" if passed else "unknown","reason":"checked original request","evidence":["proof.txt"] if passed else []},"candidate":candidate,"verdict":"satisfied" if passed else "needs_work", "summary":"independent review", "next_step":"" if passed else "verify the missing outcome", "blocker":None,
                        "criteria":[{"criterion_id":c["id"],"status":"pass" if passed else "unknown","reason":"checked concrete evidence","evidence":["proof.txt"] if passed else []} for c in contract["criteria"]],
                        "constraints":[{"constraint":c,"status":"pass" if passed else "unknown","reason":"checked","evidence":["proof.txt"] if passed else []} for c in contract["constraints"]]}
                calls = []
                if passed:
                    calls.append({"index":0,"id":f"read-{turn}","type":"function","function":{"name":"Read","arguments":json.dumps({"path":"proof.txt"})}})
                calls.append({"index":len(calls),"id":f"review-{turn}","type":"function","function":{"name":"GoalReview","arguments":json.dumps(args)}})
                emit({"tool_calls":calls})
                emit({}, "tool_calls")
            else:
                emit({"content":"execution or review round ended naturally"})
                emit({}, "stop")
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def api(base, method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.status, json.loads(resp.read())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read())


def main():
    if not os.path.exists(SERVER):
        print("build mink-server first")
        return 1
    prov = http.server.HTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=prov.serve_forever, daemon=True).start()
    prov_port = prov.server_address[1]

    port = free_port()
    home = tempfile.mkdtemp(prefix="mink-e2e-home-")
    workspace = tempfile.mkdtemp(prefix="mink-e2e-work-")
    with open(os.path.join(workspace, "proof.txt"), "w") as fh:
        fh.write("objective evidence\n")
    env = dict(os.environ)
    env.update({
        "HOME": home,
        "DEEPSEEK_API_KEY": "sk-synthetic",
        "DEEPSEEK_BASE_URL": f"http://127.0.0.1:{prov_port}",
        "MINK_SERVER_HOST": "127.0.0.1",
        "MINK_SERVER_PORT": str(port),
        "MINK_SERVER_MAX_RUNNING": "4",
        "TERM": "dumb",
    })
    proc = subprocess.Popen([SERVER], env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, cwd=workspace)
    base = f"http://127.0.0.1:{port}"
    try:
        for _ in range(80):
            try:
                with urllib.request.urlopen(base + "/health", timeout=1):
                    break
            except Exception:
                time.sleep(0.25)
        else:
            print("server never became healthy")
            print(proc.stdout.read().decode()[-2000:])
            return 1

        status, body = api(base, "POST", "/api/sessions",
                           {"name": "e2e", "cwd": workspace})
        assert status == 200, (status, body)
        sid = body["data"]["id"]
        print(f"session created: {sid}")

        status, body = api(base, "POST", f"/api/sessions/{sid}/open")
        assert status == 200, (status, body)
        print("session opened")

        events = []
        stop = threading.Event()

        def listen():
            try:
                with urllib.request.urlopen(base + f"/api/sessions/{sid}/stream", timeout=60) as r:
                    for raw in r:
                        if stop.is_set():
                            return
                        line = raw.decode("utf-8", "replace").strip()
                        if not line.startswith("data:"):
                            continue
                        try:
                            events.append(json.loads(line[5:].strip()))
                        except Exception:
                            pass
            except Exception:
                pass

        threading.Thread(target=listen, daemon=True).start()
        time.sleep(0.6)

        status, body = api(base, "POST", f"/api/sessions/{sid}/goal",
                           {"objective": "ship the parser",
                            "protocol_version": 2,
                            "generate": False,
                            "acceptance": "agent_reviewed",
                            "max_turns": 6,
                            "acceptance_criteria": ["ship the parser"]})
        assert status == 200, (status, body)
        goal = body["data"]["goal"]
        assert goal["status"] == "draft"
        status, body = api(base, "POST", f"/api/sessions/{sid}/goal/confirm", {"goal_id":goal["id"],"revision":goal["revision"]})
        assert status == 200, (status, body)
        goal = body["data"]["goal"]
        gid = goal["id"]
        print(f"goal created: {gid} revision={goal['revision']} status={goal['status']}")

        # Give the driver real turns to run.
        final = None
        for _ in range(60):
            time.sleep(0.5)
            st, snap = api(base, "GET", f"/api/sessions/{sid}/goal")
            final = snap["data"]["goal"]
            if final and final["status"] in ("completed", "failed", "blocked", "budget_exhausted"):
                break
        print(f"goal final status: {final['status']} after {len(final.get('iterations') or [])} iterations")
        print(f"provider calls: {Provider.calls}, goal continuations: {len(Provider.goal_ids)}")
        print(f"goal reason: {final.get('reason')}")
        print(f"goal phase: {final.get('phase')}, basis: {final.get('completion_basis')}")

        updates = [e for e in events if e.get("type") == "goal_updated"]
        seqs = [e.get("stream_sequence") for e in updates]
        statuses = [(e.get("goal") or {}).get("status") for e in updates]
        print(f"goal_updated events: {len(updates)}")
        print(f"  statuses seen: {statuses}")
        print(f"  stream_sequence: {seqs}")
        ok = True

        def check(label, cond, detail=""):
            nonlocal ok
            print(("PASS  " if cond else "FAIL  ") + label + ((" — " + detail) if detail else ""))
            if not cond:
                ok = False

        # Cross-turn scheduling, not one long turn: distinct rounds, each with
        # its own turn id, and a final for every one.
        rounds = final.get("iterations") or []
        turn_ids = [r.get("turn_id") for r in rounds]
        distinct_rounds = len({r.get("iteration") for r in rounds})
        print(f"goal rounds: iteration numbers={sorted({r.get('iteration') for r in rounds})} turn_ids={turn_ids}")
        check("goal ran at least three distinct rounds", distinct_rounds >= 3)
        check("every round has its own turn id", len(set(turn_ids)) == len(turn_ids) and all(turn_ids))
        check("every round finished", all(r.get("finished") for r in rounds))
        print("--- last request message shapes ---")
        for row in (Provider.shapes[-1] if Provider.shapes else []):
            print("   ", json.dumps(row, ensure_ascii=False)[:180])
        check("driver actually ran goal turns", Provider.calls >= 2,
              f"provider calls={Provider.calls}")
        check("goal lifecycle reached a terminal state",
              final["status"] in ("completed", "budget_exhausted", "blocked", "failed"),
              final["status"])
        check("independent review acceptance completes the goal",
              final["status"] == "completed" and final.get("completion_basis") == "agent_reviewed",
              f"{final['status']}/{final.get('completion_basis')}")
        check("goal_updated reached the SSE stream", len(updates) >= 2,
              f"{len(updates)} events")
        check("goal updates are ordered by stream_sequence",
              all(seqs[i] < seqs[i + 1] for i in range(len(seqs) - 1)), str(seqs))
        check("goal_updated carried real states", any(s for s in statuses), str(statuses))

        # ---- Controlled budget exhaustion ----
        # A model that always reports continue must be stopped by max_turns:
        # exactly the admitted iterations run, the goal settles as
        # budget_exhausted, and no further provider request is made.
        status, body = api(base, "POST", "/api/sessions",
                           {"name": "budget", "cwd": workspace})
        assert status == 200, (status, body)
        budget_sid = body["data"]["id"]
        status, body = api(base, "POST", f"/api/sessions/{budget_sid}/open")
        assert status == 200, (status, body)
        Provider.mode = "continue"
        calls_before = Provider.calls
        status, body = api(base, "POST", f"/api/sessions/{budget_sid}/goal",
                           {"objective": "keep probing until stopped",
                            "protocol_version": 2,
                            "generate": False,
                            "acceptance": "agent_reviewed",
                            "max_turns": 2})
        assert status == 200, (status, body)
        draft = body["data"]["goal"]
        status, body = api(base, "POST", f"/api/sessions/{budget_sid}/goal/confirm", {"goal_id":draft["id"],"revision":draft["revision"]})
        assert status == 200, (status, body)
        budget = None
        for _ in range(60):
            time.sleep(0.5)
            st, snap = api(base, "GET", f"/api/sessions/{budget_sid}/goal")
            budget = snap["data"]["goal"]
            if budget and budget["status"] in ("completed", "failed", "blocked", "budget_exhausted"):
                break
        budget_rounds = budget.get("iterations") or []
        budget_calls = Provider.calls - calls_before
        print(f"budget goal: status={budget['status']} rounds={len(budget_rounds)} "
              f"reason={budget.get('reason')} provider_calls={budget_calls}")
        check("a model that keeps continuing is stopped by the budget",
              budget["status"] == "budget_exhausted",
              f"{budget['status']}")
        check("budget stop admits exactly max_turns rounds", len(budget_rounds) == 2,
              f"{len(budget_rounds)} rounds")
        # Three requests per admitted round: execution, review submission and audit ending.
        # No extra execution round is admitted after the budget.
        check("budget stop made no extra provider request", budget_calls == 3 * len(budget_rounds),
              f"{budget_calls} calls for {len(budget_rounds)} rounds")
        settled_calls = Provider.calls
        time.sleep(3)
        check("no provider request after the budget stop", Provider.calls == settled_calls,
              f"{Provider.calls - settled_calls} extra")

        stop.set()
        return 0 if ok else 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except Exception:
            proc.kill()
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(workspace, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
