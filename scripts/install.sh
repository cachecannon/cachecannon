#!/bin/bash
set -euo pipefail

BINARY="cachecannon"
APT_URL="https://apt.cachecannon.cc"
YUM_URL="https://yum.cachecannon.cc"

# --- helpers ----------------------------------------------------------------

die() { echo "Error: $*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not found"
}

# --- detect platform --------------------------------------------------------

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)   ARCH="amd64"; RPM_ARCH="x86_64" ;;
        aarch64|arm64)  ARCH="arm64"; RPM_ARCH="aarch64" ;;
        *)              die "unsupported architecture: $(uname -m)" ;;
    esac
}

detect_os() {
    if [ ! -f /etc/os-release ]; then
        die "cannot detect OS — /etc/os-release not found"
    fi
    . /etc/os-release
    case "${ID:-}" in
        ubuntu|debian|pop|linuxmint|elementary|zorin)
            PKG_TYPE="deb" ;;
        fedora|rhel|centos|rocky|alma|amzn|ol)
            PKG_TYPE="rpm" ;;
        *)
            if command -v apt-get >/dev/null 2>&1; then
                PKG_TYPE="deb"
            elif command -v dnf >/dev/null 2>&1 || command -v yum >/dev/null 2>&1; then
                PKG_TYPE="rpm"
            else
                die "unsupported distribution: ${ID:-unknown}"
            fi
            ;;
    esac
}

# --- setup repo and install -------------------------------------------------

setup_apt() {
    need curl

    echo "Adding APT repository..."
    curl -fsSL "${APT_URL}/gpg-key.asc" \
        | sudo gpg --dearmor -o /usr/share/keyrings/cachecannon-archive-keyring.gpg

    echo "deb [signed-by=/usr/share/keyrings/cachecannon-archive-keyring.gpg] ${APT_URL} stable main" \
        | sudo tee /etc/apt/sources.list.d/cachecannon.list > /dev/null

    echo "Installing ${BINARY}..."
    sudo apt-get update -qq
    sudo apt-get install -y "${BINARY}"
}

setup_yum() {
    need curl

    echo "Adding YUM repository..."
    sudo tee /etc/yum.repos.d/cachecannon.repo > /dev/null << EOF
[cachecannon]
name=Cachecannon
baseurl=${YUM_URL}/${RPM_ARCH}
gpgcheck=1
gpgkey=${YUM_URL}/RPM-GPG-KEY-cachecannon
enabled=1
EOF

    echo "Installing ${BINARY}..."
    if command -v dnf >/dev/null 2>&1; then
        sudo dnf install -y "${BINARY}"
    else
        sudo yum install -y "${BINARY}"
    fi
}

# --- main -------------------------------------------------------------------

main() {
    echo "Installing ${BINARY}..."
    detect_arch
    detect_os

    case "$PKG_TYPE" in
        deb) setup_apt ;;
        rpm) setup_yum ;;
    esac

    echo "Done — ${BINARY} installed successfully."
    echo "Future updates are available via your package manager."
}

main
