#!/usr/bin/env python3
"""生成 rsync 增量测试用的两个样本文件。

为什么需要它：`test_librsync_e2e_v1_to_v2_must_match_exactly` 需要一对
"内容相近、大小不同"的二进制文件来验证 Signature → Delta → Patch 的端到端正确性。
这两个文件各 10MB，直接进仓库太重（20MB），而且**内容是什么完全不重要** ——
所以改成用脚本现生成。测试在文件不存在时会打印 SKIP 而不是失败，不会阻塞别人。

用法：
    python tests/make_testdata.py
"""
import os
import random

HERE = os.path.dirname(os.path.abspath(__file__))
TD = os.path.join(HERE, "testdata")

V1_SIZE = 10 * 1024 * 1024          # 10 MiB
APPEND = 100 * 1024                 # v2 比 v1 多 100 KiB（与仓库里原来的大小一致）
CHANGED_BLOCKS = 40                 # 中间改 40 处，模拟"局部修改"


def main():
    os.makedirs(TD, exist_ok=True)
    # 固定种子，保证每次生成的内容一样（方便复现）
    rnd = random.Random(20260815)
    v1 = bytearray(rnd.getrandbits(8) for _ in range(V1_SIZE))
    v2 = bytearray(v1)
    # 在中间散布若干处修改，每处 4KB
    for i in range(CHANGED_BLOCKS):
        off = (V1_SIZE // (CHANGED_BLOCKS + 2)) * (i + 1)
        v2[off:off + 4096] = bytes(rnd.getrandbits(8) for _ in range(4096))
    # 尾部追加一段，让两个文件大小不同
    v2 += bytes(rnd.getrandbits(8) for _ in range(APPEND))

    p1 = os.path.join(TD, "file_v1.bin")
    p2 = os.path.join(TD, "file_v2.bin")
    with open(p1, "wb") as f:
        f.write(v1)
    with open(p2, "wb") as f:
        f.write(v2)
    print(f"生成 {p1}  {len(v1):,} 字节")
    print(f"生成 {p2}  {len(v2):,} 字节")
    print("现在可以跑：cargo test --features rsync")


if __name__ == "__main__":
    main()
