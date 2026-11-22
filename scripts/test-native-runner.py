#!/usr/bin/env python3
"""Exercise a Cargo install from outside the checkout without using the live daemon."""
import os
from pathlib import Path
import resource
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def cpu_limit():
    # The runner inherits this short budget; exercise actual runaway recovery
    # without waiting for the normal 120-second workspace lifetime limit.
    soft, hard = resource.getrlimit(resource.RLIMIT_CPU)
    limit = min([3] + [n for n in (soft, hard) if n != resource.RLIM_INFINITY])
    resource.setrlimit(resource.RLIMIT_CPU, (limit, limit))


def main():
    # Keep the socket path short enough for sockaddr_un on macOS.
    with tempfile.TemporaryDirectory(prefix="cant-", dir="/tmp") as tmp:
        stage = Path(tmp)
        install = stage / "install"
        subprocess.run(
            ["cargo", "install", "--locked", "--path", str(ROOT / "crates/cli"),
             "--root", str(install), "--target-dir", str(ROOT / "target")],
            cwd=ROOT, check=True,
        )
        cant = install / "bin/cant"
        runner = install / "bin/cant-steel-runner"
        assert cant.is_file() and runner.is_file(), "Cargo must install both binaries"
        env = dict(os.environ, CANT_SOCKET=str(stage / "cant.sock"), CANT_CONFIG=str(stage / "config.json"))
        env.pop("CANT_STEEL_RUNNER", None)

        def call(*args, expect=0):
            result = subprocess.run([cant, *args], cwd=stage, env=env, capture_output=True,
                                    text=True, timeout=30, preexec_fn=cpu_limit)
            assert result.returncode == expect, (result.stdout, result.stderr)
            return result.stdout

        try:
            moved_runner = runner.with_suffix(".disabled")
            runner.rename(moved_runner)
            try:
                assert "runner not found" in call("repl", "(+ 2 3)", expect=1)
            finally:
                moved_runner.rename(runner)
            assert "⇒ 5" in call("repl", "(+ 2 3)")
            (stage / "sentinel").write_text("host-callback-ok")
            assert "host-callback-ok" in call("repl", '(get (files/read "sentinel") (quote text))')
            assert "lost" in call("repl", "(let loop () (loop))", expect=1)
            assert "⇒ 7" in call("repl", "(+ 3 4)")
        finally:
            call("shutdown")
    print("Cargo install: both binaries, sibling discovery, eval, host callback, CPU interruption, recovery OK")


if __name__ == "__main__":
    main()
