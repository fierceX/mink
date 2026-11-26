import ast
import asyncio
import inspect as _py_inspect
import json
import math
import os
import sys
import threading
import traceback
import types

_PROTOCOL_VERSION = 1
_MIN_INT64 = -(1 << 63)
_MAX_UINT64 = (1 << 64) - 1
_TEXT_CHUNK_CHARS = 512
_WRITE_LOCK = threading.Lock()


class _PtcRevoked(BaseException):
    """Uncatchable cancellation raised when the host revokes execution."""


class ToolCallError(Exception):
    def __init__(self, tool_name: str, kind: str, message: str) -> None:
        super().__init__(message)
        self.tool_name = tool_name
        self.kind = kind
        self.message = message

    def __str__(self) -> str:
        return f"ToolCallError({self.tool_name}, {self.kind}): {self.message}"


class Image:
    _TOKEN = object()

    def __init__(
        self,
        token: object,
        image_id: str,
        path: str,
        fmt: str,
        width: int,
        height: int,
        byte_len: int,
    ) -> None:
        if token is not Image._TOKEN:
            raise TypeError("Image instances can only be created by tool calls")
        object.__setattr__(self, "_image_id", image_id)
        object.__setattr__(self, "_path", path)
        object.__setattr__(self, "_format", fmt)
        object.__setattr__(self, "_width", width)
        object.__setattr__(self, "_height", height)
        object.__setattr__(self, "_bytes", byte_len)

    @property
    def image_id(self) -> str:
        return self._image_id

    @property
    def path(self) -> str:
        return self._path

    @property
    def format(self) -> str:
        return self._format

    @property
    def width(self) -> int:
        return self._width

    @property
    def height(self) -> int:
        return self._height

    @property
    def bytes(self) -> int:
        return self._bytes

    def __setattr__(self, _name: str, _value: object) -> None:
        raise AttributeError("Image is immutable")

    def __repr__(self) -> str:
        return (
            f"Image(image_id={self._image_id!r}, format={self._format!r}, "
            f"width={self._width}, height={self._height}, path={self._path!r})"
        )


def _canonical_json(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _validate_lossless(value: object, depth: int = 0) -> object:
    if depth > 64:
        raise ValueError("tool arguments exceed maximum nesting depth")
    if value is None or isinstance(value, (bool, str)):
        return value
    if isinstance(value, int):
        if value < _MIN_INT64 or value > _MAX_UINT64:
            raise ValueError("integer out of 64-bit JSON range")
        return value
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError("tool arguments must not contain NaN or Infinity")
        return value
    if isinstance(value, (list, tuple)):
        return [_validate_lossless(v, depth + 1) for v in value]
    if isinstance(value, dict):
        out = {}
        for k, v in value.items():
            if not isinstance(k, str):
                raise TypeError("JSON object keys must be strings")
            out[k] = _validate_lossless(v, depth + 1)
        return out
    raise TypeError(f"tool arguments must be JSON-serializable, got {type(value).__name__}")


def _open_transport():
    stdio_mode = os.environ.get("MINK_PTC_STDIO_CTRL") == "1"
    if stdio_mode:
        in_stream = sys.stdin.buffer
        out_stream = sys.stdout.buffer
        return in_stream, out_stream, True

    fd_raw = os.environ.pop("MINK_PTC_CTRL_FD", None)
    if fd_raw is None:
        raise RuntimeError("missing MINK_PTC_CTRL_FD")
    fd = int(fd_raw)
    # Keep only minimal process-essential env vars so user code sees a clean env.
    keep = {}
    for k in ("PATH", "TMPDIR", "TEMP", "TMP", "HOME", "LANG", "LC_ALL"):
        if k in os.environ:
            keep[k] = os.environ[k]
    os.environ.clear()
    os.environ.update(keep)

    ctrl_file = os.fdopen(fd, "r+b", buffering=0)
    return ctrl_file, ctrl_file, False


def _write_frame(stream, msg: dict) -> None:
    line = (json.dumps(msg, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")
    with _WRITE_LOCK:
        stream.write(line)
        if hasattr(stream, "flush"):
            stream.flush()


class _PtcStdoutWrapper:
    def __init__(self, emit_text) -> None:
        self._emit = emit_text

    def write(self, s: str) -> int:
        if s:
            self._emit(str(s))
        return len(s)

    def flush(self) -> None:
        pass

    def isatty(self) -> bool:
        return False


def _main() -> int:
    in_stream, out_stream, stdio_ctrl = _open_transport()
    _write_frame(out_stream, {"type": "ready", "protocol_version": _PROTOCOL_VERSION})

    raw_run = in_stream.readline()
    if not raw_run:
        return 1
    run_msg = json.loads(raw_run.decode("utf-8"))
    if run_msg.get("type") != "run":
        return 1

    code = run_msg["code"]
    cwd = run_msg.get("cwd") or ""
    tool_names = list(run_msg.get("tool_names") or [])
    max_output_bytes = int(run_msg.get("max_output_bytes") or 0)
    if cwd:
        os.chdir(cwd)

    output_lock = threading.Lock()
    output_discarded = False
    emitted_text_bytes = 0

    def emit_text(text: str) -> None:
        nonlocal output_discarded, emitted_text_bytes
        if not text:
            return
        with output_lock:
            if output_discarded:
                return
            for idx in range(0, len(text), _TEXT_CHUNK_CHARS):
                if output_discarded:
                    break
                chunk = text[idx : idx + _TEXT_CHUNK_CHARS]
                chunk_bytes = len(chunk.encode("utf-8"))
                _write_frame(out_stream, {"type": "output", "kind": "text", "text": chunk})
                emitted_text_bytes += chunk_bytes
                if max_output_bytes > 0 and emitted_text_bytes > max_output_bytes:
                    output_discarded = True
                    break

    def emit_image(image_id: str) -> None:
        with output_lock:
            if output_discarded:
                return
            _write_frame(out_stream, {"type": "output", "kind": "image", "image_id": image_id})

    if stdio_ctrl:
        sys.stdout = _PtcStdoutWrapper(emit_text)
        sys.stderr = _PtcStdoutWrapper(emit_text)

    async def _run_program() -> tuple[bool, dict | None]:
        nonlocal output_discarded
        loop = asyncio.get_running_loop()
        pending: dict[int, asyncio.Future] = {}
        next_call_id = 0
        revoked_reason: str | None = None
        inspectables: dict[int, tuple[object, str, str]] = {}
        main_task: asyncio.Task | None = None

        def _format_and_emit_value(val: object, seen: set[int]) -> str:
            if isinstance(val, Image):
                emit_image(val.image_id)
                return ""
            if isinstance(val, str):
                return val
            obj_id = id(val)
            if isinstance(val, (dict, list, tuple)):
                if obj_id in seen:
                    return "<recursive>"
                seen.add(obj_id)
                try:
                    if isinstance(val, (list, tuple)):
                        parts = []
                        for item in val:
                            if isinstance(item, Image):
                                emit_image(item.image_id)
                            else:
                                parts.append(_format_nested(item, seen))
                        return json.dumps(parts, ensure_ascii=False, indent=2)
                    out_dict = {}
                    for k, v in val.items():
                        if isinstance(v, Image):
                            emit_image(v.image_id)
                            out_dict[str(k)] = f"<Image {v.image_id}>"
                        else:
                            out_dict[str(k)] = _format_nested(v, seen)
                    return json.dumps(out_dict, ensure_ascii=False, indent=2)
                finally:
                    seen.discard(obj_id)
            try:
                return json.dumps(val, ensure_ascii=False, indent=2)
            except TypeError:
                return repr(val)

        def _format_nested(val: object, seen: set[int]) -> object:
            if isinstance(val, Image):
                emit_image(val.image_id)
                return f"<Image {val.image_id}>"
            if val is None or isinstance(val, (bool, int, float, str)):
                return val
            obj_id = id(val)
            if obj_id in seen:
                return "<recursive>"
            seen.add(obj_id)
            try:
                if isinstance(val, (list, tuple)):
                    return [_format_nested(item, seen) for item in val]
                if isinstance(val, dict):
                    return {str(k): _format_nested(v, seen) for k, v in val.items()}
                return repr(val)
            finally:
                seen.discard(obj_id)

        def _has_nested_image(val: object, seen: set[int] | None = None) -> bool:
            if isinstance(val, Image):
                return True
            if not isinstance(val, (dict, list, tuple)):
                return False
            if seen is None:
                seen = set()
            oid = id(val)
            if oid in seen:
                return False
            seen.add(oid)
            if isinstance(val, (list, tuple)):
                return any(_has_nested_image(x, seen) for x in val)
            return any(_has_nested_image(v, seen) for v in val.values())

        def inspect(*values: object) -> None:
            if revoked_reason is not None:
                raise _PtcRevoked(revoked_reason)
            if not values:
                return
            plain_parts: list[str] = []

            def flush_plain() -> None:
                if plain_parts:
                    emit_text(" ".join(plain_parts) + "\n")
                    plain_parts.clear()

            for val in values:
                if isinstance(val, Image):
                    flush_plain()
                    emit_image(val.image_id)
                    continue
                entry = inspectables.get(id(val))
                if entry is not None and entry[0] is val:
                    try:
                        if _canonical_json(val) == entry[1]:
                            flush_plain()
                            if entry[2]:
                                text = entry[2] if entry[2].endswith("\n") else entry[2] + "\n"
                                emit_text(text)
                            continue
                    except TypeError:
                        pass
                if _has_nested_image(val):
                    flush_plain()
                    rendered = _format_and_emit_value(val, set())
                    if rendered:
                        emit_text(rendered + ("\n" if not rendered.endswith("\n") else ""))
                    continue
                if isinstance(val, str):
                    plain_parts.append(val)
                else:
                    try:
                        plain_parts.append(json.dumps(val, ensure_ascii=False, indent=2))
                    except TypeError:
                        plain_parts.append(repr(val))
            flush_plain()

        def _make_tool_callable(tool_name: str):
            async def _call(*pos_args: object, **kwargs: object) -> object:
                nonlocal next_call_id
                if revoked_reason is not None:
                    raise _PtcRevoked(revoked_reason)
                if pos_args and kwargs:
                    raise TypeError(
                        f"tools.{tool_name}() accepts either keyword arguments or one dict argument, not both"
                    )
                if len(pos_args) > 1:
                    raise TypeError(
                        f"tools.{tool_name}() accepts at most one positional dict argument"
                    )
                if len(pos_args) == 1:
                    if not isinstance(pos_args[0], dict):
                        raise TypeError(
                            f"tools.{tool_name}() positional argument must be a dict"
                        )
                    payload = _validate_lossless(pos_args[0])
                else:
                    payload = _validate_lossless(dict(kwargs))

                next_call_id += 1
                call_id = next_call_id
                fut = loop.create_future()
                pending[call_id] = fut
                _write_frame(
                    out_stream,
                    {
                        "type": "tool_call",
                        "id": call_id,
                        "name": tool_name,
                        "args": payload,
                    },
                )
                return await fut

            _call.__name__ = tool_name
            return _call

        class _ToolsProxy:
            def __init__(self, allowed: list[str]) -> None:
                self._allowed = {name: _make_tool_callable(name) for name in allowed}

            def __getattr__(self, name: str):
                fn = self._allowed.get(name)
                if fn is None:
                    raise AttributeError(f"tool {name!r} is not available in this PTC session")
                return fn

            def __getitem__(self, name: str):
                fn = self._allowed.get(name)
                if fn is None:
                    raise KeyError(f"tool {name!r} is not available in this PTC session")
                return fn

            def __dir__(self) -> list[str]:
                return sorted(self._allowed.keys())

        tools_proxy = _ToolsProxy(tool_names)

        class _ToolsModule(types.ModuleType):
            def __getattr__(self, name: str):
                return getattr(tools_proxy, name)

            def __getitem__(self, name: str):
                return tools_proxy[name]

            def __dir__(self) -> list[str]:
                return dir(tools_proxy)

        tools_mod = _ToolsModule("tools")
        tools_mod.__all__ = sorted(tool_names)
        for _t_name in tool_names:
            if _t_name.isidentifier():
                setattr(tools_mod, _t_name, getattr(tools_proxy, _t_name))
        prev_tools_mod = sys.modules.get("tools")
        sys.modules["tools"] = tools_mod

        def _handle_host_line(raw_line: bytes) -> bool:
            nonlocal output_discarded, revoked_reason
            if not raw_line:
                revoked_reason = "host control channel closed"
                for fut in list(pending.values()):
                    if not fut.done():
                        fut.set_exception(_PtcRevoked(revoked_reason))
                if main_task is not None and not main_task.done():
                    main_task.cancel()
                return False
            msg = json.loads(raw_line.decode("utf-8"))
            mtype = msg.get("type")
            if mtype == "tool_reply":
                cid = msg["id"]
                fut = pending.pop(cid, None)
                if fut is None or fut.done():
                    return True
                if not msg.get("ok"):
                    err = msg.get("error") or {}
                    fut.set_exception(
                        ToolCallError(
                            err.get("tool_name", ""),
                            err.get("kind", "unknown"),
                            err.get("message", "tool call failed"),
                        )
                    )
                    return True
                img = msg.get("image")
                if img is not None:
                    fut.set_result(
                        Image(
                            Image._TOKEN,
                            img["image_id"],
                            img["path"],
                            img["format"],
                            int(img["width"]),
                            int(img["height"]),
                            int(img["bytes"]),
                        )
                    )
                    return True
                val = msg.get("value")
                insp = msg.get("inspection")
                if isinstance(val, (dict, list)) and isinstance(insp, str):
                    try:
                        if len(inspectables) > 4096:
                            inspectables.clear()
                        inspectables[id(val)] = (val, _canonical_json(val), insp)
                    except TypeError:
                        pass
                fut.set_result(val)
            elif mtype == "discard_output":
                output_discarded = True
            elif mtype == "revoke":
                revoked_reason = msg.get("reason") or "execution revoked by host"
                for fut in list(pending.values()):
                    if not fut.done():
                        fut.set_exception(_PtcRevoked(revoked_reason))
                if main_task is not None and not main_task.done():
                    main_task.cancel()
                return False
            return True

        stop_reader = False

        def _reader_thread() -> None:
            while not stop_reader:
                try:
                    line = in_stream.readline()
                except OSError:
                    line = b""
                if stop_reader:
                    break
                try:
                    loop.call_soon_threadsafe(_handle_host_line, line)
                except RuntimeError:
                    break
                if not line:
                    break

        reader = threading.Thread(target=_reader_thread, name="ptc-ctrl-reader", daemon=True)
        reader.start()
        try:
            compiled = compile(
                code,
                "<run_code>",
                "exec",
                flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT,
            )
            env = {
                "__name__": "__main__",
                "tools": tools_proxy,
                "inspect": inspect,
                "gather": asyncio.gather,
                "ToolCallError": ToolCallError,
                "Image": Image,
            }

            async def _exec_body() -> None:
                ret = eval(compiled, env, env)
                if _py_inspect.iscoroutine(ret):
                    await ret

            main_task = asyncio.create_task(_exec_body())
            await main_task
            if revoked_reason is not None:
                return False, {"kind": "abort", "message": revoked_reason}
            return True, None
        except _PtcRevoked as exc:
            return False, {"kind": "abort", "message": str(exc)}
        except asyncio.CancelledError:
            msg = revoked_reason or "execution cancelled"
            return False, {"kind": "abort", "message": msg}
        except SyntaxError as exc:
            return False, {"kind": "exception", "message": f"SyntaxError: {exc}"}
        except BaseException as exc:
            tb = traceback.format_exc()
            return False, {"kind": "exception", "message": tb.strip() or f"{type(exc).__name__}: {exc}"}
        finally:
            stop_reader = True
            if prev_tools_mod is None:
                sys.modules.pop("tools", None)
            else:
                sys.modules["tools"] = prev_tools_mod
            for fut in list(pending.values()):
                if not fut.done():
                    fut.cancel()

    ok, err = asyncio.run(_run_program())
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.flush()
        except OSError:
            pass
    done_msg: dict = {
        "type": "done",
        "ok": ok,
        "output_truncated": output_discarded,
    }
    if err is not None:
        done_msg["error"] = err
    try:
        _write_frame(out_stream, done_msg)
    except OSError:
        pass
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(_main())
