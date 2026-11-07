#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
import sys
from pathlib import Path

os.environ.setdefault("AUTARQIA_AIRLLM_BACKEND", "torch")

import airllm
import huggingface_hub
from transformers import AutoConfig, AutoTokenizer

AIRLLM_COMMIT = "c4c9de3d41e73770b95716bd447e53f049a2bc85"
PATCH_PATH = Path(__file__).resolve().parent / "patches" / "0001-safetensors-torch-backend-and-trust-policy.patch"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while True:
            chunk = f.read(1024 * 1024)
            if not chunk:
                break
            h.update(chunk)
    return "0x" + h.hexdigest()


def sha256_bytes(data: bytes) -> str:
    return "0x" + hashlib.sha256(data).hexdigest()


def resolve_revision(model_id: str, revision: str | None) -> str:
    api = huggingface_hub.HfApi()
    info = api.model_info(model_id, revision=revision)
    if not info.sha:
        raise RuntimeError(f"Unable to resolve exact commit SHA for {model_id}")
    return info.sha


def prepare_model(
    model_id: str,
    revision: str | None,
    output_dir: Path,
    trust_remote_code: bool = False,
    compression: str | None = None,
) -> dict:
    if trust_remote_code:
        raise ValueError("trust_remote_code=True is forbidden by default security policy")

    resolved_revision = resolve_revision(model_id, revision)
    output_dir = output_dir.resolve()
    checkpoint_dir = output_dir / "checkpoint"
    checkpoint_dir.mkdir(parents=True, exist_ok=True)

    huggingface_hub.snapshot_download(
        repo_id=model_id,
        revision=resolved_revision,
        local_dir=str(checkpoint_dir),
        ignore_patterns=["*.bin", "*.pt", "*.pth", "*.gguf", "*.h5", "*.msgpack"],
    )

    config_path = checkpoint_dir / "config.json"
    if not config_path.exists():
        raise FileNotFoundError(f"Missing config.json in {checkpoint_dir}")
    config_json = json.loads(config_path.read_text(encoding="utf-8"))
    if "auto_map" in config_json and not trust_remote_code:
        raise RuntimeError(
            f"Model {model_id} defines auto_map in config.json and requires remote code, which is rejected"
        )

    _ = AutoConfig.from_pretrained(
        str(checkpoint_dir),
        trust_remote_code=False,
        local_files_only=True,
    )
    tokenizer = AutoTokenizer.from_pretrained(
        str(checkpoint_dir),
        trust_remote_code=False,
        local_files_only=True,
    )

    model = airllm.AutoModel.from_pretrained(
        str(checkpoint_dir),
        layer_shards_saving_path=str(output_dir),
        compression=compression,
        trust_remote_code=False,
        local_files_only=True,
        install_hooks=False,
        load_resident=False,
        prefetching=False,
    )
    splitted_dir = Path(model.checkpoint_path).resolve()
    if not splitted_dir.exists():
        raise FileNotFoundError(f"Expected splitted_model directory at {splitted_dir}")

    original_files = {}
    for file_path in sorted(checkpoint_dir.rglob("*")):
        if not file_path.is_file():
            continue
        rel = file_path.relative_to(checkpoint_dir).as_posix()
        if rel.startswith(".cache/"):
            continue
        original_files[rel] = {
            "sha256": sha256_file(file_path),
            "size_bytes": file_path.stat().st_size,
        }

    split_shards = {}
    for shard_path in sorted(splitted_dir.glob("*.safetensors")):
        rel = shard_path.name
        done_marker = shard_path.with_name(shard_path.name + ".done")
        if not done_marker.exists():
            raise RuntimeError(f"Missing completion marker {done_marker} for split shard {shard_path}")
        split_shards[rel] = {
            "sha256": sha256_file(shard_path),
            "size_bytes": shard_path.stat().st_size,
        }

    if not split_shards:
        raise RuntimeError(f"No .safetensors shards produced in {splitted_dir}")

    tokenizer_material = b""
    for tok_file in ("tokenizer.json", "tokenizer_config.json", "vocab.json", "merges.txt"):
        p = checkpoint_dir / tok_file
        if p.exists():
            tokenizer_material += tok_file.encode("utf-8") + b"\0" + p.read_bytes() + b"\0"
    tokenizer_sha256 = sha256_bytes(tokenizer_material)

    chat_template_str = tokenizer.chat_template or ""
    if isinstance(chat_template_str, dict):
        chat_template_str = json.dumps(chat_template_str, sort_keys=True)
    chat_template_sha256 = sha256_bytes(chat_template_str.encode("utf-8"))

    original_manifest_sha256 = sha256_bytes(
        json.dumps(original_files, sort_keys=True, separators=(",", ":")).encode("utf-8")
    )
    split_manifest_sha256 = sha256_bytes(
        json.dumps(split_shards, sort_keys=True, separators=(",", ":")).encode("utf-8")
    )
    patch_sha256 = sha256_file(PATCH_PATH) if PATCH_PATH.exists() else "0x" + ("00" * 32)

    manifest = {
        "schema_version": 1,
        "engine_name": "airllm",
        "airllm_commit": AIRLLM_COMMIT,
        "patch_sha256": patch_sha256,
        "model_id": model_id,
        "model_revision": resolved_revision,
        "trust_remote_code": False,
        "compression": compression,
        "checkpoint_dir": str(checkpoint_dir),
        "splitted_model_dir": str(splitted_dir),
        "tokenizer_sha256": tokenizer_sha256,
        "chat_template_sha256": chat_template_sha256,
        "original_manifest_sha256": original_manifest_sha256,
        "split_manifest_sha256": split_manifest_sha256,
        "original_files": original_files,
        "split_shards": split_shards,
    }
    manifest_path = output_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return manifest


def main() -> int:
    parser = argparse.ArgumentParser(description="Offline AirLLM model downloader, layer splitter, and manifest builder.")
    parser.add_argument("--model-id", default="Qwen/Qwen2.5-0.5B-Instruct", help="HuggingFace model ID")
    parser.add_argument("--revision", default=None, help="Exact HuggingFace revision / commit SHA")
    parser.add_argument(
        "--output-dir",
        default=None,
        help="Destination directory for checkpoint, splitted_model, and manifest.json",
    )
    parser.add_argument("--compression", default=None, choices=[None, "4bit", "8bit"])
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[2]
    safe_slug = args.model_id.replace("/", "--")
    output_dir = Path(args.output_dir) if args.output_dir else (repo_root / "data" / "models" / safe_slug)

    manifest = prepare_model(
        model_id=args.model_id,
        revision=args.revision,
        output_dir=output_dir,
        trust_remote_code=False,
        compression=args.compression,
    )
    print(
        json.dumps(
            {
                "model_id": manifest["model_id"],
                "model_revision": manifest["model_revision"],
                "tokenizer_sha256": manifest["tokenizer_sha256"],
                "chat_template_sha256": manifest["chat_template_sha256"],
                "original_manifest_sha256": manifest["original_manifest_sha256"],
                "split_manifest_sha256": manifest["split_manifest_sha256"],
                "split_shard_count": len(manifest["split_shards"]),
                "manifest_path": str(output_dir.resolve() / "manifest.json"),
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
