#!/usr/bin/env python3
"""Generate target-specific release notices without compiling dependency code."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
TARGETS = (
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin", "aarch64-apple-darwin", "x86_64-pc-windows-gnu",
)
LIMIT = 2 << 20


def normalized(text):
    return "\n".join(line.rstrip() for line in text.splitlines()).strip()


def command(*arguments):
    result = subprocess.run(arguments, cwd=ROOT, capture_output=True, text=True,
                            encoding="utf-8", timeout=180)
    if result.returncode:
        raise ValueError(result.stderr.strip() or f"{arguments[0]} failed")
    return result.stdout


def crate_file(root, relative):
    path = (root / relative).resolve()
    if not path.is_relative_to(root.resolve()):
        raise ValueError(f"Crate file escapes its source directory: {relative}")
    if path.stat().st_size > LIMIT:
        raise ValueError(f"Notice input exceeds {LIMIT} bytes: {path}")
    return path.read_bytes()


def revision(root):
    commit = json.loads(crate_file(root, ".cargo_vcs_info.json"))["git"]["sha1"]
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Invalid upstream source revision")
    return commit


def runtime(config):
    release = command(os.environ.get("RUSTC", "rustc"), "--version").split()[1]
    if release != config["rust_version"]:
        raise ValueError(f"Rust {release} needs refreshed runtime notices "
                         f"(reference: {config['rust_version']})")


def packages(target):
    metadata = json.loads(command("cargo", "metadata", "--locked", "--format-version", "1",
                                  "--filter-platform", target))
    tree = command("cargo", "tree", "--locked", "--target", target, "--edges", "normal",
                   "--prefix", "none", "--format", "{p}")
    selected = {match.groups() for line in tree.splitlines()[1:]
                if (match := re.match(r"^([A-Za-z0-9_-]+) v([^ ]+)", line))}
    found = {}
    for package in metadata["packages"]:
        if package["id"] == metadata["resolve"]["root"]:
            continue
        key = (package["name"], package["version"])
        if key in selected:
            if key in found:
                raise ValueError(f"Ambiguous package sources: {key}")
            found[key] = package
    if selected != found.keys():
        raise ValueError(f"Dependency metadata is incomplete: {selected - found.keys()}")
    return metadata, [found[key] for key in sorted(selected)]


class Notices:
    def __init__(self, config):
        reference = (ROOT / "THIRD_PARTY_NOTICES.txt").read_text(encoding="utf-8")
        corpus = reference.split("\nFULL NOTICES\n", 1)[1].strip()
        if hashlib.sha256(corpus.encode()).hexdigest() != config["corpus_sha256"]:
            raise ValueError("Reference notice corpus changed; review curated notice references")
        self.blocks = []
        self.by_text = {}
        for number, block in re.findall(r"NOTICE (\d+)\n(.*?)(?=\n={72}\nNOTICE |\Z)", corpus, re.S):
            labels, text = block.split("\n\n", 1)
            if int(number) != len(self.blocks) + 1:
                raise ValueError("Reference notice numbering is not contiguous")
            self.blocks.append([labels, text.rstrip()])
            self.by_text[normalized(text)] = int(number)

    def add(self, text, label):
        text = normalized(text)
        if not text:
            raise ValueError(f"Empty notice: {label}")
        number = self.by_text.get(text)
        if number is None:
            number = len(self.blocks) + 1
            self.blocks.append(["Source:\n  " + label, text])
            self.by_text[text] = number
        return number

    def text(self, number):
        return self.blocks[number - 1][1]


def upstream_notices(package, root, config, cache):
    repository = (package.get("repository") or "").replace("http://", "https://").rstrip("/")
    rule = config["upstream"].get(repository)
    if rule is None or package["name"] not in rule["packages"]:
        return []
    commit = revision(root)
    slug = repository.removeprefix("https://github.com/")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", slug):
        raise ValueError(f"Unsupported upstream repository: {repository}")
    key = (repository, commit)
    if key not in cache:
        files = []
        for relative in rule["files"]:
            url = f"https://raw.githubusercontent.com/{slug}/{commit}/{relative}"
            request = urllib.request.Request(url, headers={"User-Agent": "memtest-notices"})
            try:
                with urllib.request.urlopen(request, timeout=30) as response:
                    data = response.read(LIMIT + 1)
            except urllib.error.HTTPError as error:
                if error.code == 404:
                    continue
                raise
            if len(data) > LIMIT:
                raise ValueError(f"Upstream notice exceeds {LIMIT} bytes: {url}")
            files.append((url, data.decode("utf-8-sig")))
        cache[key] = files
    return cache[key]


def package_notices(package, config, notices, cache):
    name, version, license = package["name"], package["version"], package["license"]
    if license not in config["license_expressions"]:
        raise ValueError(f"Unreviewed license expression for {name} {version}: {license}")
    root = Path(package["manifest_path"]).parent.resolve()
    if "LicenseRef-Slint" in license:
        paths = [root / "LICENSES/LicenseRef-Slint-Royalty-free-2.0.md"]
    else:
        paths = [path for path in root.iterdir() if path.is_file()
                 and re.match(r"(?i)^(licen[cs]e|copying|notice|unlicense|ofl)", path.name)]
        if package.get("license_file"):
            path = root / package["license_file"]
            if path.is_file():
                paths.append(path)
        if (root / "LICENSES").is_dir():
            paths += [path for path in (root / "LICENSES").rglob("*") if path.is_file()]
    files = [(path.relative_to(root).as_posix(),
              crate_file(root, path.relative_to(root)).decode("utf-8-sig"))
             for path in sorted(set(paths))]
    if not files:
        files = upstream_notices(package, root, config, cache)
    refs = [notices.add(text, f"{name} {version} / {path}") for path, text in files]
    if not refs:
        pinned = config["pinned_missing"].get(name)
        if not (pinned and version == pinned["version"] and license == pinned["license"]
                and revision(root) == pinned["commit"]):
            raise ValueError(f"No license/copyright notices found for {name} {version}")
        refs = pinned["notices"].copy()
    if package.get("repository") == "https://github.com/madsmtm/objc2":
        refs.append(config["mit_terms"])
    if "LicenseRef-Slint" in license:
        if package.get("repository") != "https://github.com/slint-ui/slint":
            raise ValueError(f"Unrecognized Slint license source for {name}")
        copyright = notices.text(config["slint_copyright"]).split("\nSource:", 1)[0]
        refs.append(notices.add(
            f"{copyright}\nSource: https://github.com/slint-ui/slint/tree/{revision(root)}\n"
            "Selected license: Slint Royalty-free Desktop, Mobile, and Web Applications License, version 2.0.",
            f"{name} {version} / source copyright"))
    return list(dict.fromkeys(refs))


def supplements(selected, config, notices):
    result = []
    for package in selected:
        name = package["name"]
        rule = config["fonts"].get(name)
        if rule is None:
            continue
        root = Path(package["manifest_path"]).parent.resolve()
        for relative, expected in rule["files"].items():
            if hashlib.sha256(crate_file(root, relative)).hexdigest() != expected:
                raise ValueError(f"Changed bundled font/notice needs review: {name}/{relative}")
        refs = rule["notices"].copy()
        if name == "sctk-adwaita":
            source = crate_file(root, "src/lib.rs").decode("utf-8").replace("\r\n", "\n")
            if rule["provenance"] not in source:
                raise ValueError("sctk-adwaita source provenance changed; review its MPL supplement")
            text = (f"sctk-adwaita {package['version']} rounded_headerbar_shape provenance:\n\n"
                    f"{rule['provenance']}\n\nSource: https://raw.githubusercontent.com/"
                    f"PolyMeilex/sctk-adwaita/{revision(root)}/src/lib.rs\n"
                    "Original project: https://gitlab.com/snakedye/snui/")
            refs += [196, notices.add(text, f"sctk-adwaita {package['version']} / source provenance")]
        result.append(f"{name} bundled font/source supplements | notices " + ", ".join(map(str, refs)))
    return result


def generate(target):
    config = json.loads((ROOT / "scripts/notices.json").read_text(encoding="utf-8"))
    runtime(config)
    metadata, selected = packages(target)
    project = next(package for package in metadata["packages"]
                   if Path(package["manifest_path"]).resolve() == ROOT / "Cargo.toml")
    notices = Notices(config)
    cache, index = {}, []
    for package in selected:
        refs = package_notices(package, config, notices, cache)
        index.append(f"{package['name']} {package['version']} | {package['license']} | notices "
                     + ", ".join(map(str, refs)))
    extra = supplements(selected, config, notices)
    lines = [f"THIRD-PARTY NOTICES - memtest {project['version']}", "",
             f"Target: {target}; locked default-GUI normal dependency edges.",
             "Build-only dependencies and inactive optional packages are excluded.",
             "Slint uses its Royalty-free Desktop, Mobile, and Web Applications License 2.0.",
             "Public README/download and release pages carry its attribution badge.", "",
             "The dependency index is generated from this package's Cargo.lock.",
             "Full texts retain audited source labels and numbering from the reference corpus;",
             "those labels record provenance, including dependencies from other supported targets.",
             "Additional current notice texts are appended. Rust and GNU runtime notices are retained.",
             "The CLI-only build has no third-party Cargo dependencies.", "",
             "DEPENDENCY INDEX", "Package/version | published SPDX expression | full notice numbers",
             *index, "", *extra,
             f"Rust {config['rust_version']} standard library | notice 232 and its referenced full texts",
             "Windows GNU runtime and MinGW-w64 attribution | notices 233, 234, 235, 236", "",
             "FULL NOTICES"]
    for number, (labels, text) in enumerate(notices.blocks, 1):
        lines += ["", "=" * 72, f"NOTICE {number}", labels, "", text]
    return "\n".join(lines) + "\n", len(selected)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        if arguments.output.resolve() == (ROOT / "THIRD_PARTY_NOTICES.txt").resolve():
            raise ValueError("Output must not overwrite the audited reference corpus")
        text, count = generate(arguments.target)
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(text, encoding="utf-8")
        print(f"{arguments.target}: {count} dependencies; wrote {arguments.output}")
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        print(f"Notice generation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
