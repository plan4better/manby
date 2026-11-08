"""Writes definitions.txt.gz, the PROJ strings of crs-defs, a line per code:
the code, a tab and the string, in the order of the codes.

    python3 src/crs/update.py 0.1.1
"""

import gzip
import json
import sys
import urllib.request

version = sys.argv[1]
url = "https://cdn.jsdelivr.net/npm/crs-defs@%s/all.json" % version
definitions = json.load(urllib.request.urlopen(url))


def key(code):
    authority, number = code.split(":")
    return (authority, int(number) if number.isdigit() else 0, number)


text = "".join("%s\t%s\n" % (c, definitions[c]) for c in sorted(definitions, key=key))
out = __file__.rsplit("/", 1)[0] + "/definitions.txt.gz"
# mtime 0, so the same data makes the same file
with open(out, "wb") as f, gzip.GzipFile(fileobj=f, mode="wb", compresslevel=9, mtime=0) as g:
    g.write(text.encode())
with open(out.rsplit("/", 1)[0] + "/VERSION", "w") as f:
    f.write(version + "\n")
print("%d definitions from crs-defs %s" % (len(definitions), version))
