#!/usr/bin/env python3
"""解析 librime-octagram 的 `.gram` 语法模型（darts-clone double-array trie）。

既是库（`GramDb`），也是自检 CLI：

    python scripts/lm/gram.py <path/to/zh-hans-t-essay-bgc.gram>

会打印 metadata、真实搭配探针的命中情况、以及全表 value 分布。

格式依据全部记在 `docs/design/language-model-integration.md` §2.2，
其中 §2.2.3a 是本文件位域运算的出处。若那份文档与本文件不一致，**以实测为准并更新文档**。
"""
import math
import struct
import sys

# 与 gram_db.cc 的 kValueScale 一致：存的是 int(ln(频次) * 10000)
VALUE_SCALE = 10000
# gram_encoding.h::kMaxEncodedUnicode
MAX_ENCODED_UNICODE = 8


def encode(s: str) -> bytes:
    """复刻 `gram_encoding.cc::encode`。

    CJK 主区（U+4000..U+A000）压成 2 字节，这是 octagram 缩小 trie 键长的手段。
    **单向映射**（u==0 与 (u&0xFF)==0 都走转义），不要拿它做往返转换。
    """
    out = bytearray()
    for ch in s:
        u = ord(ch)
        if u < 0x80:
            out.append(0xE0 if u == 0 else u)
        elif 0x4000 <= u < 0xA000:
            if (u & 0xFF) == 0:
                out.append(0xE1)
                out.append((u >> 8) + 0x40)
            else:
                out.append((u >> 8) + 0x40)
                out.append(u & 0xFF)
        else:
            bits, v = 32, u
            while bits > 0 and (v & 0xFE000000) == 0:
                bits -= 7
                v = (v << 7) & 0xFFFFFFFF
            n = (bits + 6) // 7
            out.append(0xE0 | n)
            while n > 0:
                n -= 1
                out.append(((v >> 25) & 0x7F) | 0x80)
                v = (v << 7) & 0xFFFFFFFF
    return bytes(out)


class GramDb:
    """只读打开一个 `.gram`。"""

    def __init__(self, path: str):
        blob = open(path, "rb").read()
        self.format = blob[0:32].split(b"\0")[0].decode()
        if not self.format.startswith("Rime::Grammar/"):
            raise ValueError(f"不是 gram 文件：format={self.format!r}")
        self.db_checksum, self.da_size = struct.unpack_from("<II", blob, 32)
        (off,) = struct.unpack_from("<i", blob, 40)
        start = 40 + off  # OffsetPtr 相对自身地址
        avail = len(blob) - start
        # darts-clone 的 unit 是 4 字节（不是原版 Darts 的 8 字节）。
        # 这个等式成立本身就是格式判据，不成立说明文件或理解有问题。
        if self.da_size * 4 != avail:
            raise ValueError(f"unit 大小校验失败：da_size*4={self.da_size*4} != 可用字节 {avail}")
        self.n = avail // 4
        self.arr = memoryview(blob)[start:start + self.n * 4].cast("I")
        self._blob = blob  # 保持 memoryview 有效

    # --- darts-clone 位域（见设计文档 §2.2.3a）---
    @staticmethod
    def _has_leaf(u): return ((u >> 8) & 1) == 1

    @staticmethod
    def _value(u): return u & 0x7FFFFFFF

    @staticmethod
    def _offset(u): return (u >> 10) << ((u & 0x200) >> 6)

    @staticmethod
    def _label(u): return u & 0x800000FF

    def traverse(self, key: bytes, node_pos: int = 0):
        """从 `node_pos` 沿 key 走。返回 (value, node)；失配返回 (None, None)，
        走到了但自身不成词返回 (-1, node)。"""
        idx, u = node_pos, self.arr[node_pos]
        for b in key:
            idx ^= self._offset(u) ^ b
            if idx >= self.n:
                return None, None
            u = self.arr[idx]
            if self._label(u) != b:
                return None, None
        if not self._has_leaf(u):
            return -1, idx
        return self._value(self.arr[idx ^ self._offset(u)]), idx

    def common_prefix_search(self, key: bytes, node_pos: int = 0, max_results: int = 8):
        """沿 key 走，收集途中每个成词节点。返回 [(value, 已匹配字节数), ...]。

        注意这与「子树 top-K」是两回事：本函数只走一条路径。
        """
        idx, u = node_pos, self.arr[node_pos]
        out = []
        for i, b in enumerate(key):
            idx ^= self._offset(u) ^ b
            if idx >= self.n:
                return out
            u = self.arr[idx]
            if self._label(u) != b:
                return out
            if self._has_leaf(u):
                if len(out) < max_results:
                    out.append((self._value(self.arr[idx ^ self._offset(u)]), i + 1))
        return out

    def query_pair(self, prev_char: str, next_char: str):
        """bgc 的典型查询：前一个字 + 后一个字。命中返回 ln 域分值，否则 None。"""
        _, node = self.traverse(encode(prev_char))
        if node is None:
            return None
        res = self.common_prefix_search(encode(next_char), node)
        return res[0][0] / VALUE_SCALE if res else None


def _main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    db = GramDb(sys.argv[1])
    print(f"format={db.format!r}  units={db.n}  checksum={db.db_checksum}")

    print("\n== 真实搭配探针 ==")
    probes = [("的", "时候"), ("我", "们"), ("中", "国"), ("一", "个"),
              ("没", "有"), ("这", "个"), ("是", "不是"), ("因为", "所以")]
    hit = 0
    for ctx, word in probes:
        _, node = db.traverse(encode(ctx))
        res = db.common_prefix_search(encode(word), node) if node is not None else []
        if res:
            hit += 1
            print(f"  {ctx!r:6}+{word!r:6} → " +
                  ", ".join(f"ln={v/VALUE_SCALE:.3f}(频次≈{math.exp(v/VALUE_SCALE):.4g})"
                            for v, _ in res))
        else:
            print(f"  {ctx!r:6}+{word!r:6} → 无搭配")
    print(f"命中 {hit}/{len(probes)}")

    print("\n== 全表 value 分布 ==")
    # 全表扫描会把不可达 unit 的 offset 当真而产生噪音；ln>30（频次>1e13）
    # 在任何真实语料里都不可能，据此剔除。分位数对少量噪音稳健。
    NOISE = 300_000
    vals, noise = [], 0
    for i in range(db.n):
        u = db.arr[i]
        if GramDb._has_leaf(u):
            leaf = i ^ GramDb._offset(u)
            if leaf < db.n:
                v = GramDb._value(db.arr[leaf])
                if v > NOISE:
                    noise += 1
                else:
                    vals.append(v)
    vals.sort()
    n = len(vals)
    print(f"条目 {n}（剔除噪音 {noise} = {noise/(n+noise):.3%}）")
    for q in (0.01, 0.5, 0.95, 0.9999):
        v = vals[min(int(n * q), n - 1)]
        print(f"  p{q*100:<7g} ln={v/VALUE_SCALE:6.3f}  频次≈{math.exp(v/VALUE_SCALE):.4g}")


if __name__ == "__main__":
    _main()
