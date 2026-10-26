import re, sys, os
files = ["update.rs","contract.rs","boundary.rs","write_path.rs"]
d = r"F:\demo\tools\agentkeeper\tauri-shell\src-tauri\tests"
ascii_re = re.compile(r'[A-Za-z]{3,}')
cjk_re = re.compile(r'[\u4e00-\u9fff\u3000-\u303f\uff00-\uffef]')
for f in files:
    p = os.path.join(d,f)
    n=0
    for line in open(p, encoding='utf-8'):
        t = line.strip()
        if t.startswith("//") or t.startswith("/*") or t.startswith("*"):
            if not cjk_re.search(t) and ascii_re.search(t):
                n+=1
    print(f"{f}: {n}")
