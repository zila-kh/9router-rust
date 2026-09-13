#!/usr/bin/env python3
from pathlib import Path

path = Path("rust-backend/src/db.rs")
text = path.read_text()
old = 'let db = Db::open(temp.path().join("data.sqlite")).expect("open test database");'
new = 'let db = Db::open(&temp.path().join("data.sqlite")).expect("open test database");'
if text.count(old) != 1:
    raise RuntimeError(f"expected one Db::open test match, found {text.count(old)}")
path.write_text(text.replace(old, new, 1))
