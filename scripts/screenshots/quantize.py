#!/usr/bin/env python3
"""Validate every capture before publishing the palette PNGs."""
import json
import os
from pathlib import Path
from PIL import Image

manifest = json.loads((Path(__file__).parent / "manifest.json").read_text())
source = Path(os.environ["SHOTS_CAPTURE_DIR"])
destination = Path(os.environ["SHOTS_OUT_DIR"])
staged = source.parent / "quantized"
staged.mkdir()
for entry in manifest["captures"]:
    name = entry["image"]
    assert Path(name).name == name and name.endswith(".png"), name
    with Image.open(source / name) as image:
        assert image.width == 2880 and image.height > 0, (name, image.size)
        image.convert("RGB").quantize(colors=256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE).save(staged / name, optimize=True)
destination.mkdir(parents=True, exist_ok=True)
for entry in manifest["captures"]:
    name = entry["image"]
    (destination / name).write_bytes((staged / name).read_bytes())
print(f"Published {len(manifest['captures'])} screenshots to {destination}")
