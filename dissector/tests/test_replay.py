"""Run the Lua dissector with Wireshark API doubles and real AES-CFB vectors."""
import hashlib
from pathlib import Path

from cryptography.hazmat.decrepit.ciphers.modes import CFB
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms
from lupa.lua54 import LuaRuntime


def decrypt(key, iv, data):
    decryptor = Cipher(algorithms.AES(key), CFB(iv)).decryptor()
    return decryptor.update(data) + decryptor.finalize()


def encrypt(data):
    key = hashlib.md5(b"TEST_NONCE-test-password").hexdigest().upper()[:16].encode()
    encryptor = Cipher(algorithms.AES(key), CFB(b"0123456789abcdef")).encryptor()
    return encryptor.update(data) + encryptor.finalize()


if __name__ == "__main__":
    directory = Path(__file__).resolve().parent
    runtime = LuaRuntime(encoding=None)
    runtime.globals()[b"aes_decrypt_vector"] = decrypt
    runtime.globals()[b"aes_encrypt_vector"] = encrypt
    runtime.globals()[b"dissector_path"] = str(directory.parent / "baichuan.lua").encode()
    runtime.execute((directory / "replay.lua").read_bytes())
