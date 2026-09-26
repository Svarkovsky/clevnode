#!/bin/bash
# ==============================================================================
# Script to prepare the clevnode repository for GitHub publication
# ==============================================================================
set -e

PROJECT_ROOT=$(pwd)
KEEP_SDK=false

# --- Parse Arguments ---
for arg in "$@"; do
    case "$arg" in
        --keep-sdk)
            KEEP_SDK=true
            ;;
        --help|-h)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --keep-sdk  Keep the OpenWrt SDK (tools/ folder) to avoid re-downloading"
            echo "  --help, -h  Show this help message"
            exit 0
            ;;
        *)
            echo "Error: Unknown argument '$arg'"
            exit 1
            ;;
    esac
done

echo "========================================================================"
echo "          clevnode GitHub Publication Preparation Script"
echo "========================================================================"

# --- 1. Create root LICENSE file ---
if [ -f "${PROJECT_ROOT}/leviculum/LICENSE" ]; then
    echo "[*] Copying LICENSE to root..."
    cp "${PROJECT_ROOT}/leviculum/LICENSE" "${PROJECT_ROOT}/LICENSE"
else
    echo "[!] Warning: leviculum/LICENSE not found. Please add a LICENSE file manually."
fi

# --- 2. Generate perfect .gitignore ---
echo "[*] Generating .gitignore..."
cat << 'INNER_EOF' > "${PROJECT_ROOT}/.gitignore"
# --- Build tools and SDKs ---
tools/
*.tar.xz
*.tar.gz

# --- Rust artifacts ---
leviculum/target/
leviculum/.cargo/
leviculum/libunwind.a

# --- C build artifacts ---
clevnode/clevnode
clevnode/Makefile
clevnode/libleviculum.a
clevnode/libunwind.a
*.o
*.a

# --- Runtime data, keys and storage (CRITICAL: Do not commit!) ---
clevnode/identities/
# clevnode/posts/ (not ignored to keep default index.mu)
clevnode/files/
clevnode/reticulum/storage/
clevnode/lblogd.toml
*.log

# --- Releases and Secrets ---
Releases/
push_github.sh

# --- System and IDE files ---
.DS_Store
*.swp
*~
.vscode/
.idea/
INNER_EOF

# --- 3. Clean up build artifacts and temporary files ---
echo "[*] Cleaning up build artifacts and temporary files..."

# Remove C build outputs and logs
rm -f "${PROJECT_ROOT}/clevnode/clevnode"
rm -f "${PROJECT_ROOT}/clevnode/Makefile"
rm -f "${PROJECT_ROOT}/clevnode/libleviculum.a"
rm -f "${PROJECT_ROOT}/clevnode/libunwind.a"
rm -f "${PROJECT_ROOT}/clevnode/"*.log
rm -f "${PROJECT_ROOT}/"*.log

# Remove runtime test data and keys
rm -rf "${PROJECT_ROOT}/clevnode/identities"
if [ -d "${PROJECT_ROOT}/clevnode/posts" ]; then find "${PROJECT_ROOT}/clevnode/posts" -type f ! -name "index.mu" -delete 2>/dev/null || true; fi
rm -rf "${PROJECT_ROOT}/clevnode/files"
rm -rf "${PROJECT_ROOT}/clevnode/reticulum/storage"
rm -f "${PROJECT_ROOT}/clevnode/lblogd.toml"

# Remove Rust build artifacts
rm -rf "${PROJECT_ROOT}/leviculum/target"
rm -rf "${PROJECT_ROOT}/leviculum/.cargo"
rm -f "${PROJECT_ROOT}/leviculum/libunwind.a"

# Handle tools/ SDK folder
if [ "$KEEP_SDK" = true ]; then
    echo "[*] Keeping the OpenWrt SDK (tools/ folder) as requested."
else
    echo "[*] Removing tools/ folder (OpenWrt SDK and sstrip)..."
    rm -rf "${PROJECT_ROOT}/tools"
fi

# Remove miscellaneous bak and untracked copy files
rm -f "${PROJECT_ROOT}/"*"копия"*
rm -f "${PROJECT_ROOT}/clevnode/"*"копия"*
rm -f "${PROJECT_ROOT}/clevnode/"*.bak

echo "========================================================================"
echo "Cleanup completed successfully!"
echo "========================================================================"
echo ""
echo "Current folder size:"
du -sh "${PROJECT_ROOT}"
echo ""
echo "Git Status:"
git status
echo ""
echo "Next steps to publish:"
echo "1. git add ."
echo "2. git commit -m \"Initial commit: clevnode v0.1.0 for MIPS\""
echo "3. git remote add origin https://github.com/Svarkovsky/clevnode.git"
echo "4. git push -u origin main"
echo "========================================================================"
