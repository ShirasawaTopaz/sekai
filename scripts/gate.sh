#!/usr/bin/env bash
# 统一门禁脚本：本地与 CI 共用。
#
# 用法：
#   scripts/gate.sh            # 等价于 all：fmt + check + clippy + test + js
#   scripts/gate.sh check      # 只跑编译检查（最快，改完代码先跑这个）
#   scripts/gate.sh test       # 只跑测试
#   scripts/gate.sh fmt        # 只查格式
#   scripts/gate.sh clippy     # 只跑 clippy
#   scripts/gate.sh js         # 只跑 DSH 插件语法检查 + 冒烟 + 安装器离线测试
#
# 说明：
# - 本仓库在 Windows 上需要能写 target/ 与 .git/；受限沙箱内 cargo 无法
#   spawn rustc，请在有写权限的终端（或提权执行）里运行本脚本。
# - clippy 目前不把告警当失败（sekai-router 有既有告警）；新增告警请就地修。
# - fmt 使用 `cargo fmt --check`，不会改写文件。

set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

status=0
only="${1:-all}"

step() { printf '\n==> %s\n' "$*"; }
fail() { printf 'FAILED: %s\n' "$*"; status=1; }

need() {
  case "$only" in
    all) return 0 ;;
    "$1") return 0 ;;
    *) return 1 ;;
  esac
}

warn_count() {
  # clippy 输出按 `warning:` 计行；用于对比基线（当前 5 条）。
  grep -c '^warning' || true
}

if need fmt; then
  step "cargo fmt --all -- --check"
  if cargo fmt --all -- --check; then :; else fail "cargo fmt"; fi
fi

if need check; then
  step "cargo check --workspace --all-targets"
  if cargo check --workspace --all-targets; then :; else fail "cargo check"; fi
fi

if need clippy; then
  step "cargo clippy --workspace --all-targets"
  if out=$(cargo clippy --workspace --all-targets --message-format short 2>&1); then
    printf '%s\n' "$out" | tail -40
    printf 'clippy warnings: %s\n' "$(printf '%s\n' "$out" | grep -c ': warning:' || true)"
  else
    printf '%s\n' "$out" | tail -60
    fail "cargo clippy"
  fi
fi

if need test; then
  step "cargo test --workspace"
  if cargo test --workspace; then :; else fail "cargo test"; fi
fi

if need js; then
  step "node --check plugins/sekai-dsh"
  if node --check plugins/sekai-dsh/lib/index.js \
    && node --check plugins/sekai-dsh/lib/shared.js \
    && node --check plugins/sekai-dsh/lib/router.js \
    && node --check plugins/sekai-dsh/lib/worker.js \
    && node --check plugins/sekai-dsh/scripts/smoke.mjs; then :; else fail "node --check"; fi

  step "node plugins/sekai-dsh/scripts/smoke.mjs"
  if node plugins/sekai-dsh/scripts/smoke.mjs; then :; else fail "plugin smoke"; fi

  step "node --check scripts/install-dsh-plugin.mjs"
  if node --check scripts/install-dsh-plugin.mjs \
    && node --check scripts/install-dsh-plugin.test.mjs; then :; else fail "node --check installer"; fi

  step "node --test scripts/install-dsh-plugin.test.mjs"
  # DSH 受限沙箱会禁止 `node --test` 的子进程隔离（spawn EPERM：命名管道被拒）；
  # 普通终端与 CI 用默认隔离即可。这里只在命中该环境限制时回退到进程内模式
  # （--test-isolation=none 需要较新 Node；CI 走默认隔离，不受影响）。
  installer_out=$(node --test scripts/install-dsh-plugin.test.mjs 2>&1)
  installer_rc=$?
  if [ "$installer_rc" -ne 0 ] && printf '%s\n' "$installer_out" | grep -q 'spawn EPERM'; then
    printf '受限沙箱禁止 node --test 子进程隔离（spawn EPERM），改用 --test-isolation=none\n'
    if node --test --test-isolation=none scripts/install-dsh-plugin.test.mjs; then :; else fail "installer tests"; fi
  elif [ "$installer_rc" -ne 0 ]; then
    printf '%s\n' "$installer_out" | tail -60
    fail "installer tests"
  else
    printf '%s\n' "$installer_out" | tail -20
  fi
fi

if [ "$status" -eq 0 ]; then
  printf '\nGATE OK (%s)\n' "$only"
else
  printf '\nGATE FAILED (%s)\n' "$only"
fi
exit "$status"
