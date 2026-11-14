"""Local screenshot fixture settings shared by the Python entry points."""
import os
from pathlib import Path

BASE = os.environ.get("SHOTS_BASE_URL", "http://127.0.0.1:18831")
MOCK = os.environ.get("SHOTS_MOCK_URL", "http://127.0.0.1:18832")
PASSWORD = os.environ.get("SHOTS_PASSWORD", "demo-password-2026")
WORKDIR = Path(os.environ.get("SHOTS_WORKDIR", "/arboretum/tokomotive-screenshots"))
KEYS = WORKDIR / "keys.json"
DATABASE = WORKDIR / "toko.db"
