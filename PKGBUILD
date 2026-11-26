# Maintainer: pingod <https://github.com/pingod>
pkgname=gh-proxy-manager
pkgver=3.2.0
pkgrel=1
pkgdesc="管理 yay/makepkg 与 git clone 的 GitHub 下载加速（GTK4 图形界面 + 命令行）"
arch=('x86_64' 'aarch64')
url="https://github.com/pingod/gh-proxy-manager"
license=('MIT')
depends=('gtk4>=4.10' 'libadwaita>=1.7' 'curl' 'git' 'glibc' 'gcc-libs')
makedepends=('cargo' 'rust')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('SKIP') # 发正式 release 前用 makepkg -g 生成后替换

prepare() {
  cd "$pkgname-$pkgver"
  cargo fetch --locked
}

build() {
  cd "$pkgname-$pkgver"
  cargo build --release --locked --frozen
}

check() {
  cd "$pkgname-$pkgver"
  cargo test --release --locked --frozen
}

package() {
  cd "$pkgname-$pkgver"

  install -Dm755 target/release/gh-proxy-manager \
    "$pkgdir/usr/bin/gh-proxy-manager"

  install -Dm644 assets/io.github.duter2016.GhProxyManager.desktop \
    "$pkgdir/usr/share/applications/io.github.duter2016.GhProxyManager.desktop"

  install -Dm644 assets/io.github.duter2016.GhProxyManager.svg \
    "$pkgdir/usr/share/icons/hicolor/scalable/apps/io.github.duter2016.GhProxyManager.svg"

  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"

  # pacman -R 之后手动跑一次，清理工具运行时写进 /etc 和 /usr/local 的文件
  install -Dm755 /dev/stdin "$pkgdir/var/lib/gh-proxy-manager/uninstall-cleanup.sh" <<'EOF'
#!/bin/sh
# 由 gh-proxy-manager 的 PKGBUILD 安装；等价于 `gh-proxy-manager reset --yes`
rm -f /etc/gh-proxy/prefix /etc/gh-proxy/jsdelivr /etc/gh-proxy/nofailover
rm -f /etc/makepkg.conf.d/99-gh-proxy.conf
rm -f /usr/local/bin/curl-gh
rmdir /etc/gh-proxy 2>/dev/null || true
EOF
}