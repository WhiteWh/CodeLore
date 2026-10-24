"""Embedding providers. Optional by design: a provider that does not answer is
reported once and the search goes on without it."""
from __future__ import annotations

import json
import math
import urllib.error
import urllib.request
from array import array


class Unavailable(Exception):
    pass


class NoProvider:
    name = "none"
    model = ""

    def embed(self, texts: list[str]) -> list[array]:
        raise Unavailable("embeddings are off (embed.provider = \"none\")")


class Ollama:
    name = "ollama"

    def __init__(self, url: str, model: str, timeout: float):
        self.url = url.rstrip("/")
        self.model = model
        self.timeout = timeout
        self._dead: str | None = None
        self._alive = False

    def _probe(self) -> None:
        """A server that is not there is known within `timeout`; a server that
        is there may still need seconds to load the model, so only the probe
        is held to the short timeout."""
        if self._alive:
            return
        try:
            with urllib.request.urlopen(f"{self.url}/api/version", timeout=self.timeout) as r:
                r.read()
        except (urllib.error.URLError, TimeoutError, OSError) as e:
            self._dead = f"{self.model} at {self.url} did not answer ({_why(e)})"
            raise Unavailable(self._dead) from e
        self._alive = True

    def embed(self, texts: list[str], batch: int = 32) -> list[array]:
        if self._dead:
            raise Unavailable(self._dead)
        self._probe()
        outv: list[array] = []
        for i in range(0, len(texts), batch):
            chunk = texts[i:i + batch]
            body = json.dumps({"model": self.model, "input": chunk}).encode("utf-8")
            req = urllib.request.Request(f"{self.url}/api/embed", data=body,
                                         headers={"Content-Type": "application/json"})
            try:
                with urllib.request.urlopen(req, timeout=60.0) as resp:
                    data = json.loads(resp.read().decode("utf-8"))
            except (urllib.error.URLError, TimeoutError, OSError, json.JSONDecodeError) as e:
                self._dead = f"{self.model} at {self.url} did not answer ({_why(e)})"
                raise Unavailable(self._dead) from e
            vecs = data.get("embeddings")
            if not isinstance(vecs, list) or len(vecs) != len(chunk):
                self._dead = f"{self.model} at {self.url} answered without embeddings: {str(data)[:200]}"
                raise Unavailable(self._dead)
            outv.extend(normalise(v) for v in vecs)
        return outv


def _why(e: Exception) -> str:
    if isinstance(e, urllib.error.HTTPError):
        try:
            return f"HTTP {e.code}: {e.read().decode('utf-8', 'replace')[:200]}"
        except Exception:
            return f"HTTP {e.code}"
    if isinstance(e, urllib.error.URLError):
        return str(e.reason)
    return type(e).__name__ + (f": {e}" if str(e) else "")


def normalise(v) -> array:
    a = array("f", v)
    n = math.sqrt(sum(x * x for x in a)) or 1.0
    return array("f", (x / n for x in a))


def dot(a: array, b: array) -> float:
    return sum(x * y for x, y in zip(a, b))


def provider(cfg) -> NoProvider | Ollama:
    if cfg.embed_provider == "none":
        return NoProvider()
    return Ollama(cfg.embed_url, cfg.embed_model, cfg.embed_timeout)
