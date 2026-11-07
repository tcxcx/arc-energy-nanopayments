#!/usr/bin/env python3
import argparse
import concurrent.futures
import hashlib
import json
import os
import platform
import queue
import sys
import threading
import time
import uuid
from dataclasses import dataclass
from pathlib import Path

os.environ.setdefault("AUTARQIA_AIRLLM_BACKEND", "torch")

import airllm
from airllm.airllm_base import AirLLMBaseModel
from airllm.persist import ModelPersister
import grpc
import psutil
import torch
import transformers
from transformers import StoppingCriteria, StoppingCriteriaList

PROTO_ROOT = Path(__file__).resolve().parent / "autarqia_worker" / "proto"
if str(PROTO_ROOT) not in sys.path:
    sys.path.insert(0, str(PROTO_ROOT))

from autarqia.worker.v1 import worker_pb2, worker_pb2_grpc


def monotonic_ms() -> int:
    return time.monotonic_ns() // 1_000_000


def resolve_deadline_ms(raw_expires_ms: int) -> int:
    if raw_expires_ms <= 0:
        return 0
    now = monotonic_ms()
    if raw_expires_ms < now // 2:
        return now + raw_expires_ms
    return raw_expires_ms


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while True:
            chunk = f.read(1024 * 1024)
            if not chunk:
                break
            h.update(chunk)
    return "0x" + h.hexdigest()


def compute_initial_prefix_state(run_id: str) -> bytes:
    return hashlib.sha256(b"autarqia:mppi:v0:prefix\x00" + run_id.encode("utf-8")).digest()


def advance_prefix_state(prev_digest: bytes, output_seq: int, token_id: int, text_delta: str) -> bytes:
    h = hashlib.sha256()
    h.update(prev_digest)
    h.update(output_seq.to_bytes(4, "big", signed=False))
    h.update(token_id.to_bytes(4, "big", signed=False))
    h.update(text_delta.encode("utf-8"))
    return h.digest()


@dataclass
class PreparedRunInput:
    prepared_input_id: str
    run_id: str
    worker_epoch: int
    owner_fence: int
    input_token_ids: list[int]
    prompt_sha256: str
    max_output_tokens: int
    temperature_milli: int
    top_p_milli: int
    seed: int
    created_at_ms: int


class ActiveRunControl:
    def __init__(self, run_id: str, prepared: PreparedRunInput, initial_allowance: worker_pb2.InternalWorkAllowance):
        self.run_id = run_id
        self.prepared = prepared
        self.cond = threading.Condition()
        self.worker_epoch = initial_allowance.worker_epoch
        self.owner_fence = initial_allowance.owner_fence
        self.admission_epoch = initial_allowance.admission_epoch
        self.allowance_seq = initial_allowance.allowance_seq
        self.max_output_seq_exclusive = initial_allowance.max_output_seq_exclusive
        self.expires_at_monotonic_ms = resolve_deadline_ms(initial_allowance.expires_at_monotonic_ms)
        self.ledger_version = initial_allowance.ledger_version
        self.cancelled = False
        self.cancel_reason = ""
        self.draining = False
        self.drain_reason = ""
        self.stop_reason: str | None = None
        self.current_output_seq = 0


class AllowanceStreamer:
    def __init__(
        self,
        tokenizer,
        ctrl: ActiveRunControl,
        event_queue: queue.Queue,
        start_ms: int,
    ):
        self.tokenizer = tokenizer
        self.ctrl = ctrl
        self.event_queue = event_queue
        self.start_ms = start_ms
        self.seen_prompt = False
        self.prefill_emitted = False
        self.generated_ids: list[int] = []
        self.decoded_text = ""
        self.prefix_state = compute_initial_prefix_state(ctrl.run_id)
        self.special_ids = set(getattr(tokenizer, "all_special_ids", []) or [])

    def emit_prefill_if_needed(self):
        if not self.prefill_emitted:
            self.prefill_emitted = True
            self.event_queue.put(
                worker_pb2.GenerateEvent(
                    run_id=self.ctrl.run_id,
                    worker_epoch=self.ctrl.worker_epoch,
                    admission_epoch=self.ctrl.admission_epoch,
                    prefill_completed=worker_pb2.PrefillCompletedEvent(
                        input_tokens_processed=len(self.ctrl.prepared.input_token_ids),
                        elapsed_monotonic_ms=max(0, monotonic_ms() - self.start_ms),
                    ),
                )
            )

    def put(self, value):
        if not self.seen_prompt:
            self.seen_prompt = True
            return

        self.emit_prefill_if_needed()

        if isinstance(value, torch.Tensor):
            flat = value.detach().to("cpu").view(-1).tolist()
        elif isinstance(value, list):
            flat = [int(x) for x in value]
        else:
            flat = [int(value)]

        for token_id in flat:
            with self.ctrl.cond:
                next_seq = self.ctrl.current_output_seq + 1
                now_ms = monotonic_ms()
                if self.ctrl.cancelled:
                    self.ctrl.stop_reason = self.ctrl.stop_reason or f"cancelled:{self.ctrl.cancel_reason}"
                    return
                if self.ctrl.draining:
                    self.ctrl.stop_reason = self.ctrl.stop_reason or f"drained:{self.ctrl.drain_reason}"
                    return
                if self.ctrl.expires_at_monotonic_ms > 0 and now_ms > self.ctrl.expires_at_monotonic_ms:
                    self.ctrl.stop_reason = self.ctrl.stop_reason or "allowance_expired"
                    return
                if next_seq >= self.ctrl.max_output_seq_exclusive:
                    self.ctrl.stop_reason = self.ctrl.stop_reason or "allowance_exhausted"
                    return
                allowance_seq_used = self.ctrl.allowance_seq
                max_seq_excl = self.ctrl.max_output_seq_exclusive
                self.ctrl.current_output_seq = next_seq

            self.generated_ids.append(int(token_id))
            full_text = self.tokenizer.decode(
                self.generated_ids,
                skip_special_tokens=True,
                clean_up_tokenization_spaces=False,
            )
            if full_text.startswith(self.decoded_text):
                text_delta = full_text[len(self.decoded_text) :]
            else:
                text_delta = self.tokenizer.decode(
                    [int(token_id)],
                    skip_special_tokens=True,
                    clean_up_tokenization_spaces=False,
                )
            self.decoded_text = full_text

            self.prefix_state = advance_prefix_state(
                self.prefix_state,
                next_seq,
                int(token_id),
                text_delta,
            )
            commitment_hex = "0x" + self.prefix_state.hex()
            is_special = int(token_id) in self.special_ids

            self.event_queue.put(
                worker_pb2.GenerateEvent(
                    run_id=self.ctrl.run_id,
                    worker_epoch=self.ctrl.worker_epoch,
                    admission_epoch=self.ctrl.admission_epoch,
                    token_delta=worker_pb2.TokenDeltaEvent(
                        output_seq=next_seq,
                        token_id=int(token_id),
                        text_delta=text_delta,
                        prefix_commitment=commitment_hex,
                        is_special_token=is_special,
                        allowance_seq_used=allowance_seq_used,
                        elapsed_monotonic_ms=max(0, monotonic_ms() - self.start_ms),
                    ),
                )
            )

            if next_seq + 1 >= max_seq_excl:
                self.event_queue.put(
                    worker_pb2.GenerateEvent(
                        run_id=self.ctrl.run_id,
                        worker_epoch=self.ctrl.worker_epoch,
                        admission_epoch=self.ctrl.admission_epoch,
                        low_headroom=worker_pb2.LowHeadroomEvent(
                            current_output_seq=next_seq,
                            max_output_seq_exclusive=max_seq_excl,
                            allowance_seq=allowance_seq_used,
                        ),
                    )
                )

    def end(self):
        self.emit_prefill_if_needed()


class AllowanceStoppingCriteria(StoppingCriteria):
    def __init__(self, ctrl: ActiveRunControl, grace_wait_ms: int = 120):
        super().__init__()
        self.ctrl = ctrl
        self.grace_wait_ms = grace_wait_ms

    def __call__(self, input_ids: torch.LongTensor, scores: torch.FloatTensor, **kwargs) -> bool:
        with self.ctrl.cond:
            if self.ctrl.cancelled:
                self.ctrl.stop_reason = self.ctrl.stop_reason or f"cancelled:{self.ctrl.cancel_reason}"
                return True
            if self.ctrl.draining:
                self.ctrl.stop_reason = self.ctrl.stop_reason or f"drained:{self.ctrl.drain_reason}"
                return True

            now_ms = monotonic_ms()
            if self.ctrl.expires_at_monotonic_ms > 0 and now_ms > self.ctrl.expires_at_monotonic_ms:
                self.ctrl.stop_reason = self.ctrl.stop_reason or "allowance_expired"
                return True

            next_seq = self.ctrl.current_output_seq + 1
            if next_seq >= self.ctrl.max_output_seq_exclusive:
                if next_seq > self.ctrl.prepared.max_output_tokens:
                    self.ctrl.stop_reason = self.ctrl.stop_reason or "completed_max_tokens"
                    return True

                deadline_ms = now_ms + self.grace_wait_ms
                if self.ctrl.expires_at_monotonic_ms > 0:
                    deadline_ms = min(deadline_ms, self.ctrl.expires_at_monotonic_ms)

                while next_seq >= self.ctrl.max_output_seq_exclusive and not (
                    self.ctrl.cancelled or self.ctrl.draining
                ):
                    rem_ms = deadline_ms - monotonic_ms()
                    if rem_ms <= 0:
                        break
                    self.ctrl.cond.wait(timeout=rem_ms / 1000.0)

                if self.ctrl.cancelled:
                    self.ctrl.stop_reason = self.ctrl.stop_reason or f"cancelled:{self.ctrl.cancel_reason}"
                    return True
                if self.ctrl.draining:
                    self.ctrl.stop_reason = self.ctrl.stop_reason or f"drained:{self.ctrl.drain_reason}"
                    return True
                if next_seq >= self.ctrl.max_output_seq_exclusive:
                    self.ctrl.stop_reason = self.ctrl.stop_reason or "allowance_exhausted"
                    return True

            return False


class AirLlmWorkerServicer(worker_pb2_grpc.AirLlmWorkerServiceServicer):
    def __init__(
        self,
        model_dir: Path,
        worker_id: str,
        worker_epoch: int,
        device_override: str | None = None,
        tested_context_limit: int = 2048,
        max_prefill_tokens: int = 1536,
        verify_hashes: bool = True,
        grace_wait_ms: int = 120,
    ):
        self.model_dir = model_dir.resolve()
        self.manifest_path = self.model_dir / "manifest.json"
        if not self.manifest_path.exists():
            raise FileNotFoundError(
                f"Model manifest not found at {self.manifest_path}. Run prepare_model.py first."
            )
        self.manifest = json.loads(self.manifest_path.read_text(encoding="utf-8"))
        if self.manifest.get("trust_remote_code", False):
            raise RuntimeError("Refusing to load model manifest with trust_remote_code=True")

        self.checkpoint_dir = Path(self.manifest["checkpoint_dir"]).resolve()
        self.splitted_dir = Path(self.manifest["splitted_model_dir"]).resolve()
        if not self.checkpoint_dir.exists() or not self.splitted_dir.exists():
            raise FileNotFoundError("Checkpoint or splitted_model directory missing")

        if verify_hashes:
            for shard_name, meta in self.manifest["split_shards"].items():
                shard_path = self.splitted_dir / shard_name
                done_path = self.splitted_dir / (shard_name + ".done")
                if not shard_path.exists() or not done_path.exists():
                    raise FileNotFoundError(f"Missing split shard or .done marker: {shard_path}")
                actual_hash = sha256_file(shard_path)
                if actual_hash != meta["sha256"]:
                    raise RuntimeError(
                        f"Split shard integrity mismatch for {shard_name}: expected {meta['sha256']}, got {actual_hash}"
                    )

        self.worker_id = worker_id
        self.worker_epoch = int(worker_epoch)
        self.owner_fence = 0
        self.tested_context_limit = int(tested_context_limit)
        self.max_prefill_tokens = int(max_prefill_tokens)
        self.uninterruptible_step_tokens = 1
        self.grace_wait_ms = int(grace_wait_ms)

        self.model = airllm.AutoModel.from_pretrained(
            str(self.checkpoint_dir),
            layer_shards_saving_path=str(self.model_dir),
            device=device_override,
            max_seq_len=self.tested_context_limit,
            trust_remote_code=False,
            local_files_only=True,
            prefetching=True,
        )
        if not isinstance(self.model, AirLLMBaseModel):
            raise RuntimeError(f"Expected AirLLMBaseModel instance, got {type(self.model)!r}")
        persister_name = type(ModelPersister.get_model_persister()).__name__
        if persister_name != "SafetensorModelPersister":
            raise RuntimeError(f"Expected SafetensorModelPersister, got {persister_name}")

        self.tokenizer = self.model.tokenizer
        self.runtime_device = str(self.model.running_device)
        self.runtime_dtype = str(self.model.running_dtype).replace("torch.", "")

        self._lock = threading.Lock()
        self._gen_semaphore = threading.Semaphore(1)
        self._prepared_inputs: dict[str, PreparedRunInput] = {}
        self._active_run: ActiveRunControl | None = None
        self._draining = False

    def DescribeWorker(self, request, context):
        with self._lock:
            ready = not self._draining
        return worker_pb2.DescribeWorkerResponse(
            worker_id=self.worker_id,
            worker_epoch=self.worker_epoch,
            engine_name="airllm",
            airllm_commit=self.manifest["airllm_commit"],
            patch_sha256=self.manifest["patch_sha256"],
            model_id=self.manifest["model_id"],
            model_revision=self.manifest["model_revision"],
            tokenizer_sha256=self.manifest["tokenizer_sha256"],
            chat_template_sha256=self.manifest["chat_template_sha256"],
            original_manifest_sha256=self.manifest["original_manifest_sha256"],
            split_manifest_sha256=self.manifest["split_manifest_sha256"],
            runtime_device=self.runtime_device,
            runtime_dtype=self.runtime_dtype,
            tested_context_limit=self.tested_context_limit,
            max_prefill_tokens=self.max_prefill_tokens,
            uninterruptible_step_tokens=self.uninterruptible_step_tokens,
            trust_remote_code=False,
            ready=ready,
            python_version=platform.python_version(),
            torch_version=torch.__version__,
            transformers_version=transformers.__version__,
        )

    def Prepare(self, request: worker_pb2.PrepareRequest, context):
        with self._lock:
            if request.worker_epoch != self.worker_epoch:
                context.abort(
                    grpc.StatusCode.FAILED_PRECONDITION,
                    f"worker_epoch mismatch: request={request.worker_epoch} active={self.worker_epoch}",
                )
            if request.owner_fence < self.owner_fence:
                context.abort(
                    grpc.StatusCode.FAILED_PRECONDITION,
                    f"stale owner_fence: request={request.owner_fence} active={self.owner_fence}",
                )
            self.owner_fence = request.owner_fence
            if self._draining:
                context.abort(grpc.StatusCode.UNAVAILABLE, "worker is draining")

        messages = [{"role": m.role, "content": m.content} for m in request.messages]
        if not messages:
            context.abort(grpc.StatusCode.INVALID_ARGUMENT, "messages must not be empty")

        if getattr(self.tokenizer, "chat_template", None):
            rendered = self.tokenizer.apply_chat_template(
                messages,
                tokenize=True,
                add_generation_prompt=True,
            )
            if isinstance(rendered, dict) and "input_ids" in rendered:
                token_ids = [int(x) for x in rendered["input_ids"]]
            elif hasattr(rendered, "input_ids"):
                token_ids = [int(x) for x in rendered.input_ids]
            else:
                token_ids = [int(x) for x in rendered]
        else:
            joined = "\n".join(f"{m['role']}: {m['content']}" for m in messages) + "\nassistant:"
            token_ids = [int(x) for x in self.tokenizer.encode(joined)]

        input_count = len(token_ids)
        if input_count == 0:
            context.abort(grpc.StatusCode.INVALID_ARGUMENT, "tokenized input is empty")
        if input_count > self.max_prefill_tokens:
            context.abort(
                grpc.StatusCode.INVALID_ARGUMENT,
                f"input_token_count {input_count} exceeds max_prefill_tokens {self.max_prefill_tokens}",
            )
        max_out = max(1, int(request.max_output_tokens))
        if input_count + max_out > self.tested_context_limit:
            context.abort(
                grpc.StatusCode.INVALID_ARGUMENT,
                f"total requested context {input_count + max_out} exceeds tested_context_limit {self.tested_context_limit}",
            )

        canonical_payload = json.dumps(
            {"messages": messages, "input_token_ids": token_ids},
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
        prompt_sha256 = "0x" + hashlib.sha256(canonical_payload).hexdigest()
        prepared_input_id = f"prep_{uuid.uuid4().hex}"

        prepared = PreparedRunInput(
            prepared_input_id=prepared_input_id,
            run_id=request.run_id,
            worker_epoch=request.worker_epoch,
            owner_fence=request.owner_fence,
            input_token_ids=token_ids,
            prompt_sha256=prompt_sha256,
            max_output_tokens=max_out,
            temperature_milli=request.temperature_milli,
            top_p_milli=request.top_p_milli,
            seed=request.seed,
            created_at_ms=monotonic_ms(),
        )
        with self._lock:
            if len(self._prepared_inputs) >= 128:
                oldest_key = min(
                    self._prepared_inputs.keys(),
                    key=lambda k: self._prepared_inputs[k].created_at_ms,
                )
                self._prepared_inputs.pop(oldest_key, None)
            self._prepared_inputs[prepared_input_id] = prepared

        return worker_pb2.PrepareResponse(
            prepared_input_id=prepared_input_id,
            run_id=request.run_id,
            input_token_count=input_count,
            input_token_ids=token_ids,
            prompt_sha256=prompt_sha256,
            tested_context_limit=self.tested_context_limit,
        )

    def Generate(self, request: worker_pb2.GenerateRequest, context):
        init_allow = request.initial_allowance
        with self._lock:
            if init_allow.worker_epoch != self.worker_epoch:
                context.abort(
                    grpc.StatusCode.FAILED_PRECONDITION,
                    f"worker_epoch mismatch: {init_allow.worker_epoch} != {self.worker_epoch}",
                )
            if init_allow.owner_fence < self.owner_fence:
                context.abort(
                    grpc.StatusCode.FAILED_PRECONDITION,
                    f"stale owner_fence: {init_allow.owner_fence} < {self.owner_fence}",
                )
            self.owner_fence = init_allow.owner_fence
            if self._draining:
                context.abort(grpc.StatusCode.UNAVAILABLE, "worker is draining")
            prepared = self._prepared_inputs.get(request.prepared_input_id)
            if prepared is None or prepared.run_id != request.run_id:
                context.abort(
                    grpc.StatusCode.NOT_FOUND,
                    f"prepared_input_id {request.prepared_input_id} not found for run {request.run_id}",
                )

        if not self._gen_semaphore.acquire(blocking=False):
            context.abort(grpc.StatusCode.RESOURCE_EXHAUSTED, "worker busy with another active run")

        ctrl = ActiveRunControl(request.run_id, prepared, init_allow)
        with self._lock:
            self._active_run = ctrl

        event_queue: queue.Queue = queue.Queue()
        start_ms = monotonic_ms()
        streamer = AllowanceStreamer(self.tokenizer, ctrl, event_queue, start_ms)
        criteria = AllowanceStoppingCriteria(ctrl, grace_wait_ms=self.grace_wait_ms)

        def run_generation():
            try:
                if prepared.seed > 0:
                    torch.manual_seed(prepared.seed)
                input_tensor = torch.tensor(
                    [prepared.input_token_ids],
                    dtype=torch.long,
                    device=self.model.device,
                )
                attention_mask = torch.ones_like(input_tensor, device=self.model.device)

                if ctrl.max_output_seq_exclusive <= 1:
                    with torch.no_grad():
                        _ = self.model.forward(
                            input_ids=input_tensor,
                            attention_mask=attention_mask,
                            use_cache=False,
                        )
                    streamer.emit_prefill_if_needed()
                    with ctrl.cond:
                        ctrl.stop_reason = ctrl.stop_reason or "allowance_exhausted"
                else:
                    do_sample = prepared.temperature_milli > 0
                    gen_kwargs = {
                        "input_ids": input_tensor,
                        "attention_mask": attention_mask,
                        "max_new_tokens": prepared.max_output_tokens,
                        "do_sample": do_sample,
                        "streamer": streamer,
                        "stopping_criteria": StoppingCriteriaList([criteria]),
                        "use_cache": True,
                        "pad_token_id": (
                            self.tokenizer.pad_token_id
                            if self.tokenizer.pad_token_id is not None
                            else self.tokenizer.eos_token_id
                        ),
                    }
                    if do_sample:
                        gen_kwargs["temperature"] = max(0.01, prepared.temperature_milli / 1000.0)
                        if prepared.top_p_milli > 0:
                            gen_kwargs["top_p"] = min(1.0, max(0.01, prepared.top_p_milli / 1000.0))

                    with torch.no_grad():
                        _ = self.model.generate(**gen_kwargs)

                with ctrl.cond:
                    reason = ctrl.stop_reason
                    out_count = ctrl.current_output_seq
                    if reason is None:
                        if out_count >= prepared.max_output_tokens:
                            status = "completed_max_tokens"
                            detail = "reached requested max_output_tokens"
                        else:
                            status = "completed"
                            detail = "eos_or_natural_completion"
                    elif reason.startswith("cancelled:"):
                        status = "cancelled"
                        detail = reason.split(":", 1)[1]
                    elif reason.startswith("drained:"):
                        status = "drained"
                        detail = reason.split(":", 1)[1]
                    else:
                        status = reason
                        detail = reason

                event_queue.put(
                    worker_pb2.GenerateEvent(
                        run_id=ctrl.run_id,
                        worker_epoch=ctrl.worker_epoch,
                        admission_epoch=ctrl.admission_epoch,
                        terminal=worker_pb2.TerminalEvent(
                            status=status,
                            input_tokens_processed=len(prepared.input_token_ids),
                            output_tokens_generated=out_count,
                            final_prefix_commitment="0x" + streamer.prefix_state.hex(),
                            detail=detail,
                            elapsed_monotonic_ms=max(0, monotonic_ms() - start_ms),
                        ),
                    )
                )
            except Exception as exc:
                event_queue.put(
                    worker_pb2.GenerateEvent(
                        run_id=ctrl.run_id,
                        worker_epoch=ctrl.worker_epoch,
                        admission_epoch=ctrl.admission_epoch,
                        terminal=worker_pb2.TerminalEvent(
                            status="worker_error",
                            input_tokens_processed=len(prepared.input_token_ids),
                            output_tokens_generated=ctrl.current_output_seq,
                            final_prefix_commitment="0x" + streamer.prefix_state.hex(),
                            detail=f"{type(exc).__name__}: {exc}",
                            elapsed_monotonic_ms=max(0, monotonic_ms() - start_ms),
                        ),
                    )
                )
            finally:
                event_queue.put(None)

        gen_thread = threading.Thread(target=run_generation, name=f"airllm-gen-{request.run_id}", daemon=True)
        gen_thread.start()

        try:
            while True:
                if not context.is_active():
                    with ctrl.cond:
                        ctrl.cancelled = True
                        ctrl.cancel_reason = "rpc_client_disconnected"
                        ctrl.cond.notify_all()
                try:
                    ev = event_queue.get(timeout=0.1)
                except queue.Empty:
                    continue
                if ev is None:
                    break
                yield ev
        finally:
            with ctrl.cond:
                ctrl.cancelled = True
                ctrl.cancel_reason = ctrl.cancel_reason or "stream_closed"
                ctrl.cond.notify_all()
            gen_thread.join(timeout=30.0)
            with self._lock:
                if self._active_run is ctrl:
                    self._active_run = None
                self._prepared_inputs.pop(request.prepared_input_id, None)
            self._gen_semaphore.release()

    def UpdateAllowance(self, request: worker_pb2.UpdateAllowanceRequest, context):
        allow = request.allowance
        with self._lock:
            ctrl = self._active_run
            if ctrl is None or ctrl.run_id != allow.run_id:
                return worker_pb2.UpdateAllowanceResponse(
                    accepted=False,
                    rejection_reason="run_not_active",
                    active_allowance_seq=0,
                    active_max_output_seq_exclusive=0,
                )

        with ctrl.cond:
            if allow.worker_epoch != self.worker_epoch or allow.worker_epoch != ctrl.worker_epoch:
                return worker_pb2.UpdateAllowanceResponse(
                    accepted=False,
                    rejection_reason="stale_worker_epoch",
                    active_allowance_seq=ctrl.allowance_seq,
                    active_max_output_seq_exclusive=ctrl.max_output_seq_exclusive,
                )
            if allow.owner_fence < ctrl.owner_fence:
                return worker_pb2.UpdateAllowanceResponse(
                    accepted=False,
                    rejection_reason="stale_owner_fence",
                    active_allowance_seq=ctrl.allowance_seq,
                    active_max_output_seq_exclusive=ctrl.max_output_seq_exclusive,
                )
            if allow.admission_epoch != ctrl.admission_epoch:
                return worker_pb2.UpdateAllowanceResponse(
                    accepted=False,
                    rejection_reason="stale_admission_epoch",
                    active_allowance_seq=ctrl.allowance_seq,
                    active_max_output_seq_exclusive=ctrl.max_output_seq_exclusive,
                )
            if allow.allowance_seq <= ctrl.allowance_seq:
                return worker_pb2.UpdateAllowanceResponse(
                    accepted=False,
                    rejection_reason="non_monotonic_allowance_seq",
                    active_allowance_seq=ctrl.allowance_seq,
                    active_max_output_seq_exclusive=ctrl.max_output_seq_exclusive,
                )
            if allow.max_output_seq_exclusive < ctrl.max_output_seq_exclusive:
                return worker_pb2.UpdateAllowanceResponse(
                    accepted=False,
                    rejection_reason="decreasing_max_output_seq_exclusive",
                    active_allowance_seq=ctrl.allowance_seq,
                    active_max_output_seq_exclusive=ctrl.max_output_seq_exclusive,
                )
            if allow.ledger_version < ctrl.ledger_version:
                return worker_pb2.UpdateAllowanceResponse(
                    accepted=False,
                    rejection_reason="stale_ledger_version",
                    active_allowance_seq=ctrl.allowance_seq,
                    active_max_output_seq_exclusive=ctrl.max_output_seq_exclusive,
                )

            ctrl.owner_fence = allow.owner_fence
            ctrl.allowance_seq = allow.allowance_seq
            ctrl.max_output_seq_exclusive = allow.max_output_seq_exclusive
            ctrl.expires_at_monotonic_ms = resolve_deadline_ms(allow.expires_at_monotonic_ms)
            ctrl.ledger_version = allow.ledger_version
            ctrl.cond.notify_all()
            return worker_pb2.UpdateAllowanceResponse(
                accepted=True,
                rejection_reason="",
                active_allowance_seq=ctrl.allowance_seq,
                active_max_output_seq_exclusive=ctrl.max_output_seq_exclusive,
            )

    def Cancel(self, request: worker_pb2.CancelRequest, context):
        with self._lock:
            ctrl = self._active_run
            if ctrl is None or ctrl.run_id != request.run_id:
                return worker_pb2.CancelResponse(cancelled=False, stopped_at_output_seq=0)

        with ctrl.cond:
            if request.worker_epoch != ctrl.worker_epoch or request.owner_fence < ctrl.owner_fence:
                return worker_pb2.CancelResponse(
                    cancelled=False,
                    stopped_at_output_seq=ctrl.current_output_seq,
                )
            ctrl.owner_fence = request.owner_fence
            ctrl.cancelled = True
            ctrl.cancel_reason = request.reason or "explicit_cancel"
            ctrl.cond.notify_all()
            return worker_pb2.CancelResponse(
                cancelled=True,
                stopped_at_output_seq=ctrl.current_output_seq,
            )

    def Drain(self, request: worker_pb2.DrainRequest, context):
        with self._lock:
            if request.worker_epoch != self.worker_epoch or request.owner_fence < self.owner_fence:
                context.abort(grpc.StatusCode.FAILED_PRECONDITION, "stale worker_epoch or owner_fence")
            self.owner_fence = request.owner_fence
            self._draining = True
            ctrl = self._active_run
            active_run_id = ctrl.run_id if ctrl is not None else ""

        if ctrl is not None:
            with ctrl.cond:
                ctrl.draining = True
                ctrl.drain_reason = request.reason or "worker_drain"
                ctrl.cond.notify_all()

        return worker_pb2.DrainResponse(draining=True, active_run_id=active_run_id)

    def Health(self, request: worker_pb2.HealthRequest, context):
        with self._lock:
            ctrl = self._active_run
            active_run_id = ctrl.run_id if ctrl is not None else ""
            draining = self._draining
        rss = psutil.Process(os.getpid()).memory_info().rss
        return worker_pb2.HealthResponse(
            healthy=True,
            draining=draining,
            worker_epoch=self.worker_epoch,
            active_run_id=active_run_id,
            rss_bytes=rss,
            runtime_device=self.runtime_device,
        )


def serve(
    uds_path: Path,
    model_dir: Path,
    worker_id: str,
    worker_epoch: int,
    device: str | None,
    tested_context_limit: int,
    max_prefill_tokens: int,
    verify_hashes: bool,
    grace_wait_ms: int,
):
    uds_path = uds_path.resolve()
    uds_path.parent.mkdir(parents=True, exist_ok=True)
    if uds_path.exists() or uds_path.is_socket():
        uds_path.unlink()

    servicer = AirLlmWorkerServicer(
        model_dir=model_dir,
        worker_id=worker_id,
        worker_epoch=worker_epoch,
        device_override=device,
        tested_context_limit=tested_context_limit,
        max_prefill_tokens=max_prefill_tokens,
        verify_hashes=verify_hashes,
        grace_wait_ms=grace_wait_ms,
    )

    server = grpc.server(concurrent.futures.ThreadPoolExecutor(max_workers=8))
    worker_pb2_grpc.add_AirLlmWorkerServiceServicer_to_server(servicer, server)
    bind_uri = f"unix://{uds_path}"
    bound_port = server.add_insecure_port(bind_uri)
    if bound_port == 0:
        raise RuntimeError(f"Failed to bind gRPC Unix domain socket at {bind_uri}")
    server.start()
    os.chmod(uds_path, 0o600)
    print(
        json.dumps(
            {
                "event": "airllm_worker_started",
                "uds_path": str(uds_path),
                "worker_id": servicer.worker_id,
                "worker_epoch": servicer.worker_epoch,
                "model_id": servicer.manifest["model_id"],
                "model_revision": servicer.manifest["model_revision"],
                "runtime_device": servicer.runtime_device,
                "runtime_dtype": servicer.runtime_dtype,
            }
        ),
        flush=True,
    )
    server.wait_for_termination()


def main() -> int:
    parser = argparse.ArgumentParser(description="Autarqia AirLLM gRPC Unix-Domain-Socket Worker")
    parser.add_argument("--uds-path", required=True, help="Filesystem path for private Unix domain socket")
    parser.add_argument("--model-dir", required=True, help="Prepared model directory containing manifest.json")
    parser.add_argument("--worker-id", default="airllm-worker-0", help="Worker identifier")
    parser.add_argument("--worker-epoch", type=int, default=1, help="Monotonic worker epoch")
    parser.add_argument("--device", default=None, help="Optional torch device override (e.g., cpu, mps, cuda:0)")
    parser.add_argument("--tested-context-limit", type=int, default=2048)
    parser.add_argument("--max-prefill-tokens", type=int, default=1536)
    parser.add_argument("--no-verify-hashes", action="store_true")
    parser.add_argument("--grace-wait-ms", type=int, default=120)
    args = parser.parse_args()

    serve(
        uds_path=Path(args.uds_path),
        model_dir=Path(args.model_dir),
        worker_id=args.worker_id,
        worker_epoch=args.worker_epoch,
        device=args.device,
        tested_context_limit=args.tested_context_limit,
        max_prefill_tokens=args.max_prefill_tokens,
        verify_hashes=not args.no_verify_hashes,
        grace_wait_ms=args.grace_wait_ms,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
