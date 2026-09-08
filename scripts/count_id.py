#!/usr/bin/env python3
import struct, sys

path = sys.argv[1] if len(sys.argv) > 1 else "seg_00000.bin"

with open(path, "rb") as f:
    magic = f.read(8)
    assert magic == b"CDOCLOG4", f"bad magic: {magic}"

    put_count = 0
    delete_count = 0
    put_bytes = 0
    delete_bytes = 0
    while True:
        op = f.read(1)
        if not op:
            break
        op = op[0]
        start = f.tell()
        if op == 1:  # OP_PUT
            put_count += 1
            ext_id_len = struct.unpack("<I", f.read(4))[0]
            f.read(ext_id_len)          # external_id
            f.read(8)                   # internal_id (u64)
            f.read(1)                   # format byte
            src_len = struct.unpack("<I", f.read(4))[0]
            f.read(src_len)             # source bytes
            field_count = struct.unpack("<I", f.read(4))[0]
            for _ in range(field_count):
                name_len = struct.unpack("<I", f.read(4))[0]
                f.read(name_len)
                val_len = struct.unpack("<I", f.read(4))[0]
                f.read(val_len)
            put_bytes += f.tell() - start
        elif op == 2:  # OP_DELETE
            delete_count += 1
            ext_id_len = struct.unpack("<I", f.read(4))[0]
            f.read(ext_id_len)
            delete_bytes += f.tell() - start
        else:
            print(f"unknown op byte {op} at offset {f.tell()-1}", file=sys.stderr)
            break

    total_bytes = put_bytes + delete_bytes
    print(f"OP_PUT count: {put_count}   bytes: {put_bytes:,}")
    print(f"OP_DELETE count: {delete_count}   bytes: {delete_bytes:,}")
    print(f"total bytes accounted: {total_bytes:,}")
    if total_bytes:
        print(f"delete byte ratio: {delete_bytes/total_bytes:.4f}")