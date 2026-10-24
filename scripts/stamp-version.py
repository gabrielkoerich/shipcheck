# Writes the release version into Cargo.toml and the shipcheck entry of Cargo.lock
import re, sys

v = sys.argv[1].removeprefix("v") if len(sys.argv) > 1 else ""
# An empty version silently writes `version = ""`, which fails much further along
if not v:
    print("::error::the version job produced no version")
    sys.exit(1)

t = open("Cargo.toml").read()
t = re.sub(r'^version = .*$', f'version = "{v}"', t, count=1, flags=re.M)
open("Cargo.toml", "w").write(t)
l = open("Cargo.lock").read()
l = re.sub(r'(^name = "shipcheck"\n)version = .*$',
           lambda m: m.group(1) + f'version = "{v}"', l, count=1, flags=re.M)
open("Cargo.lock", "w").write(l)
