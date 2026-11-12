"""Download each repo's latest release tag as a tarball; record tag and commit."""
import json
import os
import subprocess
import sys
import urllib.request

OUT = sys.argv[1]
REPOS = sys.argv[2:]
TOKEN = subprocess.run(["gh", "auth", "token"], capture_output=True, text=True).stdout.strip()


def api(path):
    req = urllib.request.Request(
        "https://api.github.com/" + path,
        headers={"Authorization": "Bearer " + TOKEN, "Accept": "application/vnd.github+json"},
    )
    return json.load(urllib.request.urlopen(req, timeout=60))


path = os.path.join(OUT, "tags.json")
records = json.load(open(path)) if os.path.exists(path) else {}
for repo in REPOS:
    rel = api(f"repos/{repo}/releases/latest")
    tag = rel["tag_name"]
    ref = api(f"repos/{repo}/git/ref/tags/{tag}")["object"]
    sha = ref["sha"]
    if ref["type"] == "tag":
        sha = api(f"repos/{repo}/git/tags/{sha}")["object"]["sha"]
    name = repo.split("/")[1]
    dest = os.path.join(OUT, name)
    os.makedirs(dest, exist_ok=True)
    tarball = os.path.join(OUT, name + ".tar.gz")
    subprocess.run(
        ["curl", "-sSL", "-H", "Authorization: Bearer " + TOKEN, "-o", tarball,
         f"https://api.github.com/repos/{repo}/tarball/{tag}"],
        check=True,
    )
    subprocess.run(["tar", "-xzf", tarball, "-C", dest, "--strip-components", "1"], check=True)
    os.remove(tarball)
    records[repo] = {"tag": tag, "commit": sha, "published": rel["published_at"]}
    json.dump(records, open(path, "w"), indent=1)
    print(repo, tag, sha, rel["published_at"], flush=True)
