"""Rebuild the java-usages bundle and report what to do next.

Usage:
  python rebuild.py [jdtls-home] [--test] [--java /path/to/jdk25/bin/java]

Requires Java 25 (jdtls 1.63.0's shipped ECJ) and a JDT LS 1.63.0 install. If
jdtls-home is omitted it is discovered from JDTLS_HOME, PATH, scoop, Homebrew, or
OXidea's managed tools directory. After rebuilding the jar you MUST rebuild
OXidea (pnpm dev:app): the jar is embedded by include_bytes! at compile time, so
a jar rebuild alone does not change the running app.
"""
import argparse
import hashlib
import os
import pathlib
import platform
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent  # java/java-usages
OXIDEA = ROOT.parents[1]  # ox-idea/
LAUNCHER = "org.eclipse.equinox.launcher_*.jar"
JDTLS_URL = "https://download.eclipse.org/jdtls/milestones/1.63.0/jdt-language-server-1.63.0-202610191322.tar.gz"


def is_home(p):
    """A jdtls install root: plugins/ (or Homebrew libexec/plugins) with the launcher jar."""
    p = pathlib.Path(p)
    for plugins in (p / "plugins", p / "libexec" / "plugins"):
        if plugins.is_dir() and any(plugins.glob(LAUNCHER)):
            return True
    return False


def _managed_homes(base):
    """OXidea-managed jdtls installs under an app-data dir's tools/ (1-2 levels deep)."""
    for app in base.glob("*oxidea*"):
        tools = app / "tools"
        for depth in ("*/plugins", "*/*/plugins"):
            for launcher in tools.glob(f"{depth}/{LAUNCHER}"):
                yield launcher.parent.parent


def discover():
    env = os.environ.get("JDTLS_HOME")
    if env and is_home(env):
        return pathlib.Path(env)
    exe = shutil.which("jdtls")
    if exe:
        exe = pathlib.Path(exe).resolve()
        for cand in (exe.parent.parent, exe.parent.parent / "libexec"):
            if is_home(cand):
                return cand
    home = pathlib.Path.home()
    system = platform.system()
    candidates = []
    if system == "Windows":
        candidates.append(home / "scoop" / "apps" / "jdtls" / "current")
        local = pathlib.Path(os.environ.get("LOCALAPPDATA", str(home / "AppData" / "Local")))
        candidates.extend(_managed_homes(local))
    elif system == "Darwin":
        candidates += [pathlib.Path("/opt/homebrew/opt/jdtls/libexec"), pathlib.Path("/usr/local/opt/jdtls/libexec")]
        candidates.extend(_managed_homes(home / "Library" / "Application Support"))
    else:
        candidates.append(pathlib.Path("/usr/local/opt/jdtls/libexec"))
        candidates.extend(_managed_homes(home / ".local" / "share"))
    for c in candidates:
        if is_home(c):
            return c
    return None


def java_major(java):
    proc = subprocess.run([java, "-version"], capture_output=True, text=True)
    text = (proc.stderr or "") + (proc.stdout or "")
    m = re.search(r'version "(\d+)', text)
    return (int(m.group(1)) if m else None), text


def main():
    ap = argparse.ArgumentParser(description="Rebuild the java-usages bundle (needs Java 25 + JDT LS 1.63.0).")
    ap.add_argument("home", nargs="?", help="jdtls install root (plugins/ + config_*); auto-discovered if omitted")
    ap.add_argument("--test", action="store_true", help="also run the integration test against a real jdtls (slow)")
    ap.add_argument("--java", default="java", help="java executable of a JDK 25 (default: java on PATH)")
    args = ap.parse_args()

    home = pathlib.Path(args.home).resolve() if args.home else discover()
    if not home or not is_home(home):
        sys.exit(
            "jdtls-home not found. Pass it explicitly: python rebuild.py <jdtls-home>\n"
            f"Get JDT LS 1.63.0: {JDTLS_URL}\n"
            "Unpack it; the directory with plugins/ and config_win/ is the home."
        )
    print(f"jdtls-home: {home}")

    major, text = java_major(args.java)
    if major != 25:
        sys.exit(
            f"Java 25 required (found {major}). jdtls 1.63.0's ECJ needs it; bytecode targets 21.\n"
            f"Use --java /path/to/jdk25/bin/java, or put JDK 25 first on PATH.\n{text}"
        )
    print(f"java: {args.java} (major {major})")

    # build.py and the test invoke `java`/`javac` from PATH: put the chosen JDK 25 first.
    env = dict(os.environ)
    java_bin = pathlib.Path(shutil.which(args.java) or args.java).resolve().parent
    env["PATH"] = str(java_bin) + os.pathsep + env.get("PATH", "")

    print("Rebuilding jar ...")
    subprocess.run([sys.executable, str(ROOT / "build.py"), str(home)], check=True, env=env)

    jar = ROOT / "oxidea-java-usages.jar"
    sha = hashlib.sha256(jar.read_bytes()).hexdigest()
    print(f"\nBuilt {jar.name}\nSHA-256: {sha}\n(Record this in README.md under version 1.10.0.)")

    if args.test:
        print("\nRunning integration test (real jdtls) ...")
        subprocess.run([sys.executable, str(ROOT / "tests" / "implementations.py"), str(home)], check=True, env=env)

    print("\nNext: rebuild OXidea so include_bytes! embeds the new jar, then restart the app:")
    print(f"  cd {OXIDEA} && pnpm dev:app")


if __name__ == "__main__":
    main()
