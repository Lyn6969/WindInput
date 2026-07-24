#!/usr/bin/env bash
# 生成 GitHub Release 的 Release Notes(Markdown)。
#
#   scripts/gen-release-notes.sh [--tag v0.111.0] [--prev v0.110.0] [-o release_notes.md]
#
# 结构(与下游消费者的约定见下):
#   1. docs/release-notes/header.md   基础信息:下载表 + 指向官网文档的链接
#   2. ## 更新说明                     人工填写区,被 <!-- user-facing --> 标记包裹
#   3. <details> 变更记录              按 conventional commits 分类的提交列表(默认折叠)
#   4. docs/release-notes/footer.md   官网/文档/反馈入口
#
# ⚠️ user-facing 标记块有两个下游消费者,改动其格式前先看它们:
#   - 文档仓 scripts/sync_release_notes.py → data/releases.json → 官网 /changelog
#   - wind-setting src/update/notes.rs     → 应用内「有新版本」对话框
#   两者都把内容为「暂未填写」的块视作未填写而跳过。注意 Rust 侧是**全等**比较,
#   占位符前面不能加 `>` 等任何修饰,否则占位文本会被当成正文弹给用户。
set -euo pipefail

cd "$(dirname "$0")/.."   # 仓库根

TAG=""; PREV=""; OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --tag)  TAG="$2"; shift 2 ;;
    --prev) PREV="$2"; shift 2 ;;
    -o|--output) OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

# 本次版本:显式参数 > CI 的 tag > HEAD 恰好打了 tag > docs/VERSION(本地预览)
if [ -z "$TAG" ]; then
  if [ "${GITHUB_REF_TYPE:-}" = "tag" ] && [ -n "${GITHUB_REF_NAME:-}" ]; then
    TAG="$GITHUB_REF_NAME"
  else
    TAG="$(git describe --tags --exact-match HEAD 2>/dev/null || echo "v$(tr -d '[:space:]' < docs/VERSION)")"
  fi
fi
VERSION="${TAG#v}"

# 提交范围的终点:tag 已存在就用 tag,否则(打 tag 前的本地预览)用 HEAD
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then END="$TAG"; else END="HEAD"; fi

# 上一版本:优先按历史拓扑取「$END 的祖先里最近的 tag」——比版本号排序更准,
# 不会被旁支上手工打的 tag 干扰。describe 取不到(浅克隆/首个版本)时退回版本号排序。
if [ -z "$PREV" ]; then
  PREV="$(git describe --tags --abbrev=0 --match 'v*' "${END}^" 2>/dev/null || true)"
fi
if [ -z "$PREV" ]; then
  PREV="$(git tag --sort=-version:refname | grep -E '^v[0-9]+\.[0-9]+' | grep -vx "$TAG" | head -n1 || true)"
fi
if [ -n "$PREV" ]; then RANGE="$PREV..$END"; else RANGE="$END"; fi
echo "release notes: tag=$TAG range=$RANGE" >&2

# ---- 提交分类 -------------------------------------------------------------
# 折叠区是「完整的变更记录」,故不过滤 chore/ci/test 等类型:面向用户的摘要由
# 上面的人工填写区承担,这里要的是可追溯性。
breaking=""; feat=""; fix=""; perf=""; refactor=""; other=""
count=0
while IFS=$'\t' read -r sha msg; do
  if [ -z "${msg:-}" ]; then continue; fi   # 注意:set -e 下不能写成 `[ ] && continue`
  count=$((count + 1))
  if [[ "$msg" =~ ^([a-z]+)(\(([^\)]+)\))?(!)?:[[:space:]]*(.+)$ ]]; then
    type="${BASH_REMATCH[1]}"; scope="${BASH_REMATCH[3]}"
    bang="${BASH_REMATCH[4]}"; desc="${BASH_REMATCH[5]}"
  else
    type="_"; scope=""; bang=""; desc="$msg"
  fi
  if [ -n "$scope" ]; then entry="- **$scope**: $desc ($sha)"; else entry="- $desc ($sha)"; fi
  if [ -n "$bang" ]; then
    breaking="${breaking}${entry}"$'\n'
    continue
  fi
  case "$type" in
    feat)     feat="${feat}${entry}"$'\n' ;;
    fix)      fix="${fix}${entry}"$'\n' ;;
    perf)     perf="${perf}${entry}"$'\n' ;;
    refactor) refactor="${refactor}${entry}"$'\n' ;;
    *)        other="${other}${entry}"$'\n' ;;
  esac
done < <(git log "$RANGE" --pretty=format:'%h%x09%s' --no-merges)

changelog=""
append_cat() {
  if [ -n "$2" ]; then
    changelog="${changelog}#### $1"$'\n\n'"$2"$'\n'
  fi
}
append_cat "⚠️ 破坏性变更" "$breaking"
append_cat "新功能"       "$feat"
append_cat "问题修复"     "$fix"
append_cat "性能优化"     "$perf"
append_cat "重构"         "$refactor"
append_cat "其他变更"     "$other"
if [ -z "$changelog" ]; then changelog="首个版本。"$'\n'; fi

# ---- 组装 -----------------------------------------------------------------
REPO="${GITHUB_REPOSITORY:-huanfeng/WindInput}"
if [ -n "$PREV" ]; then
  summary="变更记录（$count 个提交，$PREV → $TAG）"
  compare="[完整对比](https://github.com/$REPO/compare/$PREV...$TAG)"
else
  summary="变更记录（$count 个提交）"
  compare=""
fi

render() {   # 输出模板文件,替换 {{VERSION}} / {{TAG}}
  [ -f "$1" ] || return 0
  local body; body="$(cat "$1")"
  body="${body//\{\{VERSION\}\}/$VERSION}"
  body="${body//\{\{TAG\}\}/$TAG}"
  printf '%s\n\n' "$body"
}

emit() {
  render docs/release-notes/header.md

  # 版本专属说明(可选):docs/release-notes/<tag>.md 或 <版本>.md
  local note="docs/release-notes/${TAG}.md"
  [ -f "$note" ] || note="docs/release-notes/${VERSION}.md"
  render "$note"

  printf '## 更新说明\n\n'
  printf '<!-- user-facing:start -->\n'
  printf '暂未填写\n'
  printf '<!-- user-facing:end -->\n\n'

  printf '<details>\n<summary>%s</summary>\n\n' "$summary"
  printf '%s' "$changelog"
  if [ -n "$compare" ]; then printf '%s\n' "$compare"; fi
  printf '\n</details>\n\n'

  if [ -f docs/release-notes/footer.md ]; then
    printf -- '---\n\n'
    render docs/release-notes/footer.md
  fi
}

if [ -n "$OUT" ]; then
  emit > "$OUT"
  echo "已写入 $OUT" >&2
else
  emit
fi
