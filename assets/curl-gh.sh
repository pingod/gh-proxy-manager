#!/bin/bash
# curl-gh —— 由 gh-proxy-manager 安装的 curl 包装器（v3）
#
# 作用：让 makepkg 的 https DLAGENT 自动走加速站。
#   * 前缀来自 /etc/gh-proxy/prefix；文件不存在/为空 = 官方直连
#   * /etc/gh-proxy/nofailover 存在 = 只用当前生效站，不自动换站
#   * 否则按「生效站 → 备选列表」顺序尝试；失败或龟速（低于可配置阈值持续 30s）就换下一站
#   * raw.githubusercontent.com 可选择走 jsDelivr CDN（/etc/gh-proxy/jsdelivr 存在时）
#   * 已带加速前缀的 URL（https://proxy/https://github.com/...）原样透传
#   * codeload / blob 链接先还原成官方下载形态，再拼接前缀
#
# URL 位置兼容两种调用方式：
#   curl-gh <url> [curl 参数...]           手工调用
#   curl-gh [curl 参数...] -o %o <url>     makepkg DLAGENT 模板，%u 在最后

set -u

# GH_PROXY_CONF_DIR 只为测试/便携场景保留覆盖能力，正常安装下就是 /etc/gh-proxy
CONF_DIR="${GH_PROXY_CONF_DIR:-/etc/gh-proxy}"

CURL=/usr/bin/curl
PREFIX_FILE="$CONF_DIR/prefix"
JSDELIVR_MARKER="$CONF_DIR/jsdelivr"
NOFAILOVER_MARKER="$CONF_DIR/nofailover"
MIRROR_LIST="${XDG_CONFIG_HOME:-$HOME/.config}/gh-proxy-manager/proxies"
SLOW_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/gh-proxy-manager/slow-speed"

# 龟速换站阈值：图形界面写入 slow-speed（KB/s，5–500），缺失或非法时用 20。
# 这个值每次调用都重新读取，所以在界面上拖动后立即生效，无需重新应用。
slow_kbps=20
if [ -r "$SLOW_FILE" ]; then
    raw="$(tr -cd '0-9' < "$SLOW_FILE" 2>/dev/null)"
    if [ -n "$raw" ] && [ "${#raw}" -le 4 ]; then
        raw=$((10#$raw))
        if [ "$raw" -ge 5 ] && [ "$raw" -le 500 ]; then
            slow_kbps="$raw"
        fi
    fi
fi
speed_limit=$((slow_kbps * 1024))
# 判定窗口固定 30 秒；GH_PROXY_SPEED_TIME 仅供测试缩短等待。
speed_time="${GH_PROXY_SPEED_TIME:-30}"
case "$speed_time" in
    ''|*[!0-9]*) speed_time=30 ;;
esac

if [ "$#" -eq 0 ]; then
    echo "用法: curl-gh <url> [curl 参数...]" >&2
    exit 2
fi

url="$1"
case "$url" in
    http://*|https://*)
        # 手工调用：URL 在首位，必须从参数里摘掉，否则 curl 会收到两个 URL，
        # `-o` 也会绑到错误的那个上。
        set -- "${@:2}"
        ;;
    *)
        # makepkg DLAGENT：%u 在末尾
        url="${@: -1}"
        set -- "${@:1:$(( $# - 1 ))}"
        ;;
esac

# 已经带加速前缀：原样下载，不再二次拼接
if [[ "$url" =~ ^https?://[^/]+/https?:// ]]; then
    exec "$CURL" "$@" "$url"
fi

# GitHub 的下载形态统一成官方链接：
#   codeload.github.com/o/r/{zip,tar.gz}/ref -> github.com/o/r/archive/ref.{zip,tar.gz}
#   github.com/o/r/blob/...                   -> github.com/o/r/raw/...
# 与 userscript/accelerator 的规范化规则保持一致。
canonicalize() {
    local url="$1" rest host path query=""
    rest="${url#*://}"
    case "$rest" in
        */*) host="${rest%%/*}"; path="/${rest#*/}" ;;
        *)   host="$rest"; path="/" ;;
    esac
    case "$path" in
        *\?*) query="?${path#*\?}"; path="${path%%\?*}" ;;
    esac

    case "$host" in
        codeload.github.com)
            local body user repo fmt ref
            body="${path#/}"
            user="${body%%/*}"; body="${body#*/}"
            repo="${body%%/*}"; body="${body#*/}"
            fmt="${body%%/*}";  ref="${body#*/}"
            if [ -z "$user" ] || [ -z "$repo" ] || [ -z "$ref" ]; then
                printf '%s' "$url"
                return
            fi
            case "$fmt" in
                zip)    printf 'https://github.com/%s/%s/archive/%s.zip' "$user" "$repo" "$ref" ;;
                tar.gz) printf 'https://github.com/%s/%s/archive/%s.tar.gz' "$user" "$repo" "$ref" ;;
                *)      printf '%s' "$url" ;;
            esac
            ;;
        github.com)
            case "$path" in
                */blob/*)
                    path="${path/\/blob\//\/raw\/}"
                    printf 'https://github.com%s%s' "$path" "$query"
                    ;;
                *) printf '%s' "$url" ;;
            esac
            ;;
        *) printf '%s' "$url" ;;
    esac
}

url="$(canonicalize "$url")"

is_github=0
is_raw=0
case "$url" in
    https://github.com/*|http://github.com/*) is_github=1 ;;
    https://codeload.github.com/*|http://codeload.github.com/*) is_github=1 ;;
    https://gist.githubusercontent.com/*|http://gist.githubusercontent.com/*) is_github=1 ;;
    https://raw.githubusercontent.com/*|http://raw.githubusercontent.com/*) is_raw=1 ;;
esac

# 只管 GitHub 的下载域名，其它链接直连
if [ "$is_github" = 0 ] && [ "$is_raw" = 0 ]; then
    exec "$CURL" "$@" "$url"
fi

# raw.githubusercontent.com/user/repo/branch/path
#   -> https://cdn.jsdelivr.net/gh/user/repo@branch/path
raw_to_jsdelivr() {
    local rest user repo branch path
    rest="${1#*://raw.githubusercontent.com/}"
    [ "$rest" = "$1" ] && return 1
    user="${rest%%/*}"
    [ "$user" = "$rest" ] && return 1
    rest="${rest#*/}"
    repo="${rest%%/*}"
    [ "$repo" = "$rest" ] && return 1
    rest="${rest#*/}"
    branch="${rest%%/*}"
    [ "$branch" = "$rest" ] && return 1
    rest="${rest#*/}"
    path="$rest"
    [ -n "$user" ] && [ -n "$repo" ] && [ -n "$branch" ] && [ -n "$path" ] || return 1
    printf 'https://cdn.jsdelivr.net/gh/%s/%s@%s/%s' "$user" "$repo" "$branch" "$path"
}

# ① raw 优先尝试 jsDelivr（失败则继续走下面的一般流程）
if [ "$is_raw" = 1 ] && [ -f "$JSDELIVR_MARKER" ]; then
    if jsd="$(raw_to_jsdelivr "$url")"; then
        "$CURL" --connect-timeout 5 --speed-limit "$speed_limit" --speed-time "$speed_time" \
                "$@" "$jsd" && exit 0
    fi
fi

# ② 加速前缀 + 自动换站
if [ -s "$PREFIX_FILE" ]; then
    prefix="$(cat "$PREFIX_FILE")"

    if [ -f "$NOFAILOVER_MARKER" ]; then
        exec "$CURL" "$@" "${prefix}${url}"
    fi

    declare -A seen=()
    candidates=()
    if [ -n "$prefix" ]; then
        seen[$prefix]=1
        candidates+=("$prefix")
    fi

    if [ -r "$MIRROR_LIST" ]; then
        while IFS= read -r line; do
            case "$line" in
                http://*|https://*) ;;
                *) continue ;;
            esac
            [ -z "${seen[$line]:-}" ] || continue
            seen[$line]=1
            candidates+=("$line")
        done < "$MIRROR_LIST"
    fi

    for candidate in "${candidates[@]}"; do
        "$CURL" --connect-timeout 5 --speed-limit "$speed_limit" --speed-time "$speed_time" \
                "$@" "${candidate}${url}" && exit 0
    done
fi

# ③ 全部失败/未开启：官方直连兜底
exec "$CURL" "$@" "$url"