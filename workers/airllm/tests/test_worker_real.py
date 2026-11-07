#!/usr/bin/env python3
import json
import os
import stat
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import grpc

WORKER_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(WORKER_ROOT))
sys.path.insert(0, str(WORKER_ROOT / "autarqia_worker" / "proto"))

from autarqia.worker.v1 import worker_pb2, worker_pb2_grpc
from server import advance_prefix_state, compute_initial_prefix_state, monotonic_ms


def run_worker_e2e_test():
    repo_root = WORKER_ROOT.parents[1]
    model_dir = repo_root / "data" / "models" / "Qwen--Qwen2.5-0.5B-Instruct"
    assert (model_dir / "manifest.json").exists(), f"Missing manifest at {model_dir / 'manifest.json'}"

    with tempfile.TemporaryDirectory(prefix="autarqia-uds-") as tmpdir:
        uds_path = Path(tmpdir) / "worker.sock"
        proc = subprocess.Popen(
            [
                sys.executable,
                str(WORKER_ROOT / "server.py"),
                "--uds-path",
                str(uds_path),
                "--model-dir",
                str(model_dir),
                "--worker-id",
                "airllm-test-0",
                "--worker-epoch",
                "7",
                "--grace-wait-ms",
                "250",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            startup_line = None
            deadline = time.time() + 45.0
            while time.time() < deadline:
                if proc.poll() is not None:
                    stderr_out = proc.stderr.read() if proc.stderr else ""
                    raise RuntimeError(f"Worker exited prematurely ({proc.returncode}): {stderr_out}")
                line = proc.stdout.readline()
                if line:
                    line = line.strip()
                    if line.startswith("{") and "airllm_worker_started" in line:
                        startup_line = json.loads(line)
                        break
            assert startup_line is not None, "Timed out waiting for airllm_worker_started event"
            assert uds_path.exists(), "UDS socket file does not exist"
            mode = stat.S_IMODE(os.stat(uds_path).st_mode)
            assert mode == 0o600, f"Expected UDS mode 0600, got {oct(mode)}"

            channel = grpc.insecure_channel(f"unix://{uds_path}")
            stub = worker_pb2_grpc.AirLlmWorkerServiceStub(channel)

            desc = stub.DescribeWorker(worker_pb2.DescribeWorkerRequest(), timeout=5.0)
            assert desc.worker_id == "airllm-test-0"
            assert desc.worker_epoch == 7
            assert desc.engine_name == "airllm"
            assert desc.airllm_commit == "c4c9de3d41e73770b95716bd447e53f049a2bc85"
            assert desc.model_id == "Qwen/Qwen2.5-0.5B-Instruct"
            assert desc.model_revision == "7ae557604adf67be50417f59c2c2f167def9a775"
            assert desc.trust_remote_code is False
            assert desc.ready is True

            health = stub.Health(worker_pb2.HealthRequest(), timeout=5.0)
            assert health.healthy is True
            assert health.draining is False
            assert health.worker_epoch == 7
            assert health.rss_bytes > 0

            run_id = "run_test_real_airllm_01"
            prep = stub.Prepare(
                worker_pb2.PrepareRequest(
                    run_id=run_id,
                    worker_epoch=7,
                    owner_fence=10,
                    messages=[
                        worker_pb2.ChatMessage(role="system", content="You are a concise solar energy assistant."),
                        worker_pb2.ChatMessage(
                            role="user",
                            content="Explain in one sentence how solar panels generate electricity.",
                        ),
                    ],
                    max_output_tokens=16,
                    temperature_milli=0,
                    top_p_milli=1000,
                    seed=42,
                ),
                timeout=10.0,
            )
            assert prep.run_id == run_id
            assert prep.input_token_count > 5
            assert len(prep.input_token_ids) == prep.input_token_count
            assert prep.prompt_sha256.startswith("0x") and len(prep.prompt_sha256) == 66

            init_allowance = worker_pb2.InternalWorkAllowance(
                run_id=run_id,
                worker_epoch=7,
                owner_fence=10,
                admission_epoch=3,
                allowance_seq=1,
                max_output_seq_exclusive=3,
                expires_at_monotonic_ms=monotonic_ms() + 60_000,
                ledger_version=1,
            )

            events = []
            extended = False
            stale_rejection = None

            for ev in stub.Generate(
                worker_pb2.GenerateRequest(
                    run_id=run_id,
                    prepared_input_id=prep.prepared_input_id,
                    initial_allowance=init_allowance,
                ),
                timeout=60.0,
            ):
                events.append(ev)
                which = ev.WhichOneof("event")
                if which == "low_headroom" and not extended:
                    extended = True
                    stale_rejection = stub.UpdateAllowance(
                        worker_pb2.UpdateAllowanceRequest(
                            allowance=worker_pb2.InternalWorkAllowance(
                                run_id=run_id,
                                worker_epoch=7,
                                owner_fence=10,
                                admission_epoch=2,
                                allowance_seq=2,
                                max_output_seq_exclusive=6,
                                expires_at_monotonic_ms=monotonic_ms() + 60_000,
                                ledger_version=2,
                            )
                        ),
                        timeout=5.0,
                    )
                    ok_update = stub.UpdateAllowance(
                        worker_pb2.UpdateAllowanceRequest(
                            allowance=worker_pb2.InternalWorkAllowance(
                                run_id=run_id,
                                worker_epoch=7,
                                owner_fence=10,
                                admission_epoch=3,
                                allowance_seq=2,
                                max_output_seq_exclusive=6,
                                expires_at_monotonic_ms=monotonic_ms() + 60_000,
                                ledger_version=2,
                            )
                        ),
                        timeout=5.0,
                    )
                    assert ok_update.accepted is True, f"UpdateAllowance failed: {ok_update}"
                    assert ok_update.active_max_output_seq_exclusive == 6

            assert stale_rejection is not None
            assert stale_rejection.accepted is False
            assert stale_rejection.rejection_reason == "stale_admission_epoch"

            prefill_events = [e.prefill_completed for e in events if e.WhichOneof("event") == "prefill_completed"]
            token_events = [e.token_delta for e in events if e.WhichOneof("event") == "token_delta"]
            terminal_events = [e.terminal for e in events if e.WhichOneof("event") == "terminal"]

            assert len(prefill_events) == 1
            assert prefill_events[0].input_tokens_processed == prep.input_token_count
            assert len(token_events) == 5, f"Expected exactly 5 tokens (seq 1..5 < 6), got {len(token_events)}"
            assert [t.output_seq for t in token_events] == [1, 2, 3, 4, 5]
            assert token_events[0].allowance_seq_used == 1
            assert token_events[1].allowance_seq_used == 1
            assert token_events[2].allowance_seq_used == 2
            assert token_events[4].allowance_seq_used == 2

            rolling = compute_initial_prefix_state(run_id)
            assembled = ""
            for t in token_events:
                rolling = advance_prefix_state(rolling, t.output_seq, t.token_id, t.text_delta)
                assert t.prefix_commitment == "0x" + rolling.hex()
                assembled += t.text_delta

            assert len(terminal_events) == 1
            term = terminal_events[0]
            assert term.status == "allowance_exhausted", f"Expected allowance_exhausted, got {term.status} ({term.detail})"
            assert term.input_tokens_processed == prep.input_token_count
            assert term.output_tokens_generated == 5
            assert term.final_prefix_commitment == "0x" + rolling.hex()
            assert len(assembled.strip()) > 0

            print(
                json.dumps(
                    {
                        "test": "airllm_worker_real_e2e",
                        "status": "PASS",
                        "runtime_device": desc.runtime_device,
                        "runtime_dtype": desc.runtime_dtype,
                        "input_tokens": prep.input_token_count,
                        "output_tokens": term.output_tokens_generated,
                        "assembled_text": assembled,
                        "final_prefix_commitment": term.final_prefix_commitment,
                        "terminal_status": term.status,
                    },
                    indent=2,
                )
            )
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                proc.kill()


if __name__ == "__main__":
    run_worker_e2e_test()
