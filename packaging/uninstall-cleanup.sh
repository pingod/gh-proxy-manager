#!/bin/sh
# 由 gh-proxy-manager 的包安装；等价于 `gh-proxy-manager reset --yes`
rm -f /etc/gh-proxy/prefix /etc/gh-proxy/jsdelivr /etc/gh-proxy/nofailover
rm -f /etc/makepkg.conf.d/99-gh-proxy.conf
rm -f /usr/local/bin/curl-gh
rmdir /etc/gh-proxy 2>/dev/null || true