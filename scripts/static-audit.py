#!/usr/bin/env python3
from pathlib import Path
import sys
pairs={')':'(',']':'[','}':'{'}
failed=False
paths=list(Path('rust-backend/src').rglob('*.rs'))+[Path('rust-backend/build.rs')]
for p in paths:
    s=p.read_text(); stack=[]; i=0; line=1; state='code'; quote=None
    while i<len(s):
        c=s[i]; n=s[i+1] if i+1<len(s) else ''
        if c=='\n': line+=1
        if state=='line':
            if c=='\n': state='code'
        elif state=='block':
            if c=='*' and n=='/': state='code'; i+=1
        elif state=='str':
            if c=='\\': i+=1
            elif c==quote: state='code'
        else:
            if c=='/' and n=='/': state='line'; i+=1
            elif c=='/' and n=='*': state='block'; i+=1
            elif c=='"': state='str'; quote=c
            elif c in '([{': stack.append((c,line))
            elif c in ')]}':
                if not stack or stack[-1][0]!=pairs[c]:
                    print(f'{p}:{line}: delimiter mismatch {c}'); failed=True; break
                stack.pop()
        i+=1
    if stack:
        print(f'{p}: unclosed delimiters {stack[-5:]}'); failed=True
if failed: sys.exit(1)
print('Rust source delimiter audit: OK')
