#!/bin/bash
# Startup fixups for Pixel's container environment
# Run this after container restart to restore symlinks and auth

set -e

echo "🦊 Running Pixel startup fixups..."

# Restore gitconfig symlink
if [ -f /zeroclaw-data/.gitconfig ] && [ ! -L /root/.gitconfig ]; then
    ln -sf /zeroclaw-data/.gitconfig /root/.gitconfig
    echo "  ✓ Symlinked /root/.gitconfig"
fi

# Restore gh CLI config symlink
if [ -d /zeroclaw-data/.config/gh ] && [ ! -L /root/.config/gh ]; then
    mkdir -p /root/.config
    rm -rf /root/.config/gh
    ln -sf /zeroclaw-data/.config/gh /root/.config/gh
    echo "  ✓ Symlinked /root/.config/gh"
fi

# Re-enable git credential helper via gh
if command -v gh &> /dev/null; then
    gh auth setup-git 2>/dev/null && echo "  ✓ gh auth setup-git complete" || echo "  ⚠ gh auth setup-git failed (may need manual auth)"
fi

echo "🦊 Startup fixups complete!"
