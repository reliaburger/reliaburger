"""Mirroring must preserve previously verified images on a failed transfer."""
import hashlib
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from fetch_guest_images import fetch_image


class Response(io.BytesIO):
    url = "https://images.example/ubuntu.img"


class GuestImageTests(unittest.TestCase):
    def test_verified_image_is_published(self):
        with tempfile.TemporaryDirectory() as directory:
            image = {"asset": "guest.img", "url": Response.url,
                     "sha256": hashlib.sha256(b"image").hexdigest()}
            with patch("urllib.request.urlopen", return_value=Response(b"image")):
                fetch_image(image, Path(directory))
            self.assertEqual((Path(directory) / "guest.img").read_bytes(), b"image")

    def test_bad_checksum_keeps_existing_image_and_cleans_staging(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "guest.img"
            path.write_bytes(b"keep")
            image = {"asset": "guest.img", "url": Response.url, "sha256": "0" * 64}
            with patch("urllib.request.urlopen", return_value=Response(b"bad")):
                with self.assertRaisesRegex(ValueError, "checksum"):
                    fetch_image(image, Path(directory))
            self.assertEqual(path.read_bytes(), b"keep")
            self.assertEqual(list(Path(directory).iterdir()), [path])
