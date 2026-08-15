#!/usr/bin/env python3
"""P0：量一量 bgc 语法模型在**我们自己的**数据上能覆盖多少转移。

对应 `docs/design/language-model-integration.md` §7 的 P0。
目的：在动引擎代码之前就把「收益上限」量出来。bgc 只有约 150 万字对、
对常用字理论组合覆盖率约 4%（§2.2.3e），若在真实语料上命中率也低，
整个方案的性价比就要重估。

两个指标：

- **词内字对**：词库里每个多字词的相邻字对。反映模型对既有词的覆盖，
  但 Viterbi 在词内部并不做选择，所以它只是参考。
- **★ 跨词边界字对**：把真实文本用词表做正向最大匹配分词，取「前词末字 + 后词首字」。
  **这才是 Viterbi 转移真正发生的地方**，也是 bigram 唯一能改变排序的位置。

用法：
    python scripts/lm/bigram_coverage.py --gram <zh-hans-t-essay-bgc.gram> \
        [--dict-dir <build_dev/data/schemas/pinyin/cn_dicts>] [--corpus <dir>]
"""
import argparse
import math
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from gram import GramDb  # noqa: E402

HAN = re.compile(r"[一-鿿]+")
REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def load_dict(dict_dir):
    """读 Rime .dict.yaml，返回 [(text, weight)]。

    ⚠️ 列序在本仓是被逐行判定的（见 docs 里 dict 列序那笔账），这里只做最保守的取用：
    取制表符分隔的第一列当 text（Rime 默认列序 text/code/weight），
    末列若是纯数字则当 weight。拿不准的行直接跳过——统计量不差这几条。
    """
    out = []
    for name in sorted(os.listdir(dict_dir)):
        if not name.endswith(".dict.yaml"):
            continue
        path = os.path.join(dict_dir, name)
        in_body = False
        with open(path, encoding="utf-8", errors="replace") as f:
            for line in f:
                if not in_body:
                    if line.strip() == "...":
                        in_body = True
                    continue
                line = line.rstrip("\n")
                if not line or line.startswith("#"):
                    continue
                cols = line.split("\t")
                text = cols[0].strip()
                if not text or not HAN.fullmatch(text):
                    continue
                w = 1
                if len(cols) >= 2:
                    tail = cols[-1].strip().replace("%", "")
                    try:
                        w = max(1, int(float(tail)))
                    except ValueError:
                        w = 1
                out.append((text, w))
    return out


def max_match(text, vocab, max_len=8):
    """正向最大匹配分词。词表命中不了就退化成单字。"""
    words, i, n = [], 0, len(text)
    while i < n:
        for L in range(min(max_len, n - i), 0, -1):
            if L == 1 or text[i:i + L] in vocab:
                words.append(text[i:i + L])
                i += L
                break
    return words


def collect_corpus(corpus_dir):
    """从 markdown 里抽连续汉字段作为真实文本语料。"""
    segs = []
    for root, _, files in os.walk(corpus_dir):
        if ".git" in root:
            continue
        for fn in files:
            if not fn.endswith(".md"):
                continue
            try:
                with open(os.path.join(root, fn), encoding="utf-8", errors="replace") as f:
                    segs.extend(HAN.findall(f.read()))
            except OSError:
                pass
    return [s for s in segs if len(s) >= 2]


def report(title, pairs, db):
    """pairs: [(前字, 后字, 权重)]"""
    total = hit = 0
    wtotal = whit = 0
    scores = []
    for a, b, w in pairs:
        total += 1
        wtotal += w
        v = db.query_pair(a, b)
        if v is not None:
            hit += 1
            whit += w
            scores.append(v)
    print(f"\n== {title} ==")
    if total == 0:
        print("  无样本")
        return
    print(f"  字对总数   {total:,}")
    print(f"  命中       {hit:,}  = {hit/total:.2%}")
    if wtotal:
        print(f"  加权命中   {whit/wtotal:.2%}   (按词频/出现次数加权)")
    if scores:
        scores.sort()
        n = len(scores)
        lo, mid, hi = scores[0], scores[n // 2], scores[-1]
        print(f"  命中分值   ln ∈ [{lo:.2f}, {hi:.2f}]  中位 {mid:.2f}"
              f"   (频次 {math.exp(lo):.3g} ~ {math.exp(hi):.3g})")
        span = hi - lo
        print(f"  ★ 分值跨度 {span:.2f} nat —— 这是模型能施加的最大排序影响力；"
              f"对比每词固定罚 WORD_PENALTY=3.0")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gram", required=True)
    ap.add_argument("--dict-dir",
                    default=os.path.join(REPO, "build_dev", "data", "schemas", "pinyin", "cn_dicts"))
    ap.add_argument("--corpus", default=os.path.join(REPO, "docs"))
    ap.add_argument("--limit-words", type=int, default=0, help="只取前 N 个词（调试用）")
    args = ap.parse_args()

    db = GramDb(args.gram)
    print(f"gram: {args.gram}  units={db.n:,}")

    if not os.path.isdir(args.dict_dir):
        print(f"⚠️ 词库目录不存在：{args.dict_dir}\n"
              f"   worktree 下 build_dev/data 通常没有 junction，请用 --dict-dir 指向主仓。")
        sys.exit(1)

    entries = load_dict(args.dict_dir)
    if args.limit_words:
        entries = entries[:args.limit_words]
    print(f"词条 {len(entries):,}")

    # 指标 1：词内相邻字对
    inner = []
    for text, w in entries:
        for i in range(len(text) - 1):
            inner.append((text[i], text[i + 1], w))
    report("词内字对（参考项：Viterbi 在词内不做选择）", inner, db)

    # 指标 2：跨词边界字对 —— 真正相关的那个
    vocab = {t for t, _ in entries if len(t) >= 2}
    segs = collect_corpus(args.corpus)
    cross = []
    for s in segs:
        words = max_match(s, vocab)
        for i in range(len(words) - 1):
            cross.append((words[i][-1], words[i + 1][0], 1))
    print(f"\n语料：{args.corpus} → {len(segs):,} 个汉字段，最大匹配后 {len(cross):,} 个词边界")
    report("★ 跨词边界字对（Viterbi 转移真正发生的位置）", cross, db)


if __name__ == "__main__":
    main()
